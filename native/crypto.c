/* SPDX-License-Identifier: Apache-2.0
 * Thin ownership boundary around pinned BoringSSL. Wire constants follow AOSP
 * pairing_auth and tls (Copyright The Android Open Source Project). */
#include <limits.h>
#include <openssl/aead.h>
#include <openssl/bio.h>
#include <openssl/bn.h>
#include <openssl/curve25519.h>
#include <openssl/evp.h>
#include <openssl/hkdf.h>
#include <openssl/mem.h>
#include <openssl/pem.h>
#include <openssl/rand.h>
#include <openssl/rsa.h>
#include <openssl/sha.h>
#include <openssl/ssl.h>
#include <openssl/x509.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    SSL_CTX *ctx;
    SSL *ssl;
    uint8_t *allowed;
    size_t count;
    int pin_peer;
    uint8_t peer[32];
} altdb_tls;

int altdb_random(uint8_t *out, size_t len) {
    return RAND_bytes(out, len);
}
void altdb_free(void *p) {
    OPENSSL_free(p);
}

static int fingerprint(EVP_PKEY *key, uint8_t out[32]) {
    uint8_t *der = NULL;
    int len = i2d_PUBKEY(key, &der);
    if (len <= 0)
        return 0;
    SHA256(der, (size_t)len, out);
    OPENSSL_free(der);
    return 1;
}

int altdb_identity(uint8_t **key_out, size_t *key_len, uint8_t **cert_out, size_t *cert_len) {
    int ok = 0;
    EVP_PKEY *key = EVP_PKEY_new();
    RSA *rsa = RSA_new();
    BIGNUM *e = BN_new();
    X509 *cert = X509_new();
    BIO *kb = BIO_new(BIO_s_mem()), *cb = BIO_new(BIO_s_mem());
    if (!key || !rsa || !e || !cert || !kb || !cb)
        goto done;
    if (!BN_set_word(e, RSA_F4) || !RSA_generate_key_ex(rsa, 2048, e, NULL) ||
        !EVP_PKEY_set1_RSA(key, rsa) || !X509_set_version(cert, 2) ||
        !ASN1_INTEGER_set(X509_get_serialNumber(cert), 1) ||
        !X509_gmtime_adj(X509_get_notBefore(cert), -86400) ||
        !X509_gmtime_adj(X509_get_notAfter(cert), 315360000L) || !X509_set_pubkey(cert, key))
        goto done;
    X509_NAME *name = X509_get_subject_name(cert);
    if (!X509_NAME_add_entry_by_txt(name, "CN", MBSTRING_ASC, (const uint8_t *)"altdb", -1, -1,
                                    0) ||
        !X509_set_issuer_name(cert, name) || !X509_sign(cert, key, EVP_sha256()) ||
        !PEM_write_bio_PrivateKey(kb, key, NULL, NULL, 0, NULL, NULL) ||
        !PEM_write_bio_X509(cb, cert))
        goto done;
    char *p;
    long len = BIO_get_mem_data(kb, &p);
    *key_out = OPENSSL_memdup(p, (size_t)len);
    *key_len = (size_t)len;
    len = BIO_get_mem_data(cb, &p);
    *cert_out = OPENSSL_memdup(p, (size_t)len);
    *cert_len = (size_t)len;
    ok = *key_out && *cert_out;
done:
    BIO_free(kb);
    BIO_free(cb);
    X509_free(cert);
    BN_free(e);
    RSA_free(rsa);
    EVP_PKEY_free(key);
    return ok;
}

/* Android RSAPublicKey is 524 bytes, little endian; validate its modulus size. */
int altdb_android_key_fingerprint(const uint8_t *raw, size_t len, uint8_t out[32]) {
    if (len != 524 || raw[0] != 64 || raw[1] || raw[2] || raw[3])
        return 0;
    uint8_t modulus[256];
    for (size_t i = 0; i < sizeof(modulus); i++)
        modulus[i] = raw[8 + 255 - i];
    uint32_t exponent = (uint32_t)raw[520] | (uint32_t)raw[521] << 8 | (uint32_t)raw[522] << 16 |
                        (uint32_t)raw[523] << 24;
    if (exponent != 65537 || !(modulus[255] & 1) || !(modulus[0] & 128))
        return 0;
    RSA *rsa = RSA_new();
    EVP_PKEY *key = EVP_PKEY_new();
    BIGNUM *n = BN_bin2bn(modulus, 256, NULL), *e = BN_new();
    int ok = 0;
    if (!rsa || !key || !n || !e || !BN_set_word(e, exponent))
        goto done;
    if (!RSA_set0_key(rsa, n, e, NULL))
        goto done;
    n = NULL;
    e = NULL;
    if (!EVP_PKEY_set1_RSA(key, rsa))
        goto done;
    ok = fingerprint(key, out);
done:
    BN_free(n);
    BN_free(e);
    RSA_free(rsa);
    EVP_PKEY_free(key);
    return ok;
}

static int verify_peer(X509_STORE_CTX *store, void *arg) {
    altdb_tls *t = arg;
    X509 *cert = X509_STORE_CTX_get0_cert(store);
    if (!cert)
        return 0;
    EVP_PKEY *key = X509_get_pubkey(cert);
    int ok = key && fingerprint(key, t->peer);
    EVP_PKEY_free(key);
    if (!ok)
        return 0;
    if (!t->pin_peer)
        return 1; /* Pairing or a kernel-verified vsock host. TLS still proves key possession. */
    for (size_t i = 0; i < t->count; i++)
        if (!CRYPTO_memcmp(t->peer, t->allowed + 32 * i, 32))
            return 1;
    return 0;
}

void altdb_tls_free(altdb_tls *t) {
    if (!t)
        return;
    SSL_free(t->ssl);
    SSL_CTX_free(t->ctx);
    OPENSSL_free(t->allowed);
    OPENSSL_free(t);
}

altdb_tls *altdb_tls_new(const uint8_t *kp, size_t kl, const uint8_t *cp, size_t cl, int pin_peer,
                         const uint8_t *allowed, size_t count) {
    if (kl > INT_MAX || cl > INT_MAX || count > 32)
        return NULL;
    altdb_tls *t = OPENSSL_zalloc(sizeof(*t));
    BIO *kb = NULL, *cb = NULL, *rb = NULL, *wb = NULL;
    EVP_PKEY *key = NULL;
    X509 *cert = NULL;
    if (!t)
        return NULL;
    t->pin_peer = pin_peer;
    t->count = count;
    t->allowed = count ? OPENSSL_memdup(allowed, count * 32) : NULL;
    if (count && !t->allowed)
        goto fail;
    kb = BIO_new_mem_buf(kp, (int)kl);
    cb = BIO_new_mem_buf(cp, (int)cl);
    if (!kb || !cb)
        goto fail;
    key = PEM_read_bio_PrivateKey(kb, NULL, NULL, NULL);
    cert = PEM_read_bio_X509(cb, NULL, NULL, NULL);
    t->ctx = SSL_CTX_new(TLS_method());
    if (!key || !cert || !t->ctx || !SSL_CTX_set_min_proto_version(t->ctx, TLS1_3_VERSION) ||
        !SSL_CTX_set_max_proto_version(t->ctx, TLS1_3_VERSION) ||
        !SSL_CTX_use_certificate(t->ctx, cert) || !SSL_CTX_use_PrivateKey(t->ctx, key) ||
        !SSL_CTX_check_private_key(t->ctx))
        goto fail;
    SSL_CTX_set_session_cache_mode(t->ctx, SSL_SESS_CACHE_OFF);
    SSL_CTX_set_options(t->ctx, SSL_OP_NO_TICKET);
    SSL_CTX_set_verify(t->ctx, SSL_VERIFY_PEER | SSL_VERIFY_FAIL_IF_NO_PEER_CERT, NULL);
    SSL_CTX_set_cert_verify_callback(t->ctx, verify_peer, t);
    /* Empty CA list asks adb to offer its default key. For paired TCP peers,
     * altdb_tls_add_ca installs an AOSP-compatible issuer list. */
    t->ssl = SSL_new(t->ctx);
    rb = BIO_new(BIO_s_mem());
    wb = BIO_new(BIO_s_mem());
    if (!t->ssl || !rb || !wb)
        goto fail;
    BIO_set_mem_eof_return(rb, -1);
    BIO_set_mem_eof_return(wb, -1);
    SSL_set_bio(t->ssl, rb, wb);
    rb = NULL;
    wb = NULL;
    SSL_set_accept_state(t->ssl);
    BIO_free(kb);
    BIO_free(cb);
    EVP_PKEY_free(key);
    X509_free(cert);
    return t;
fail:
    BIO_free(kb);
    BIO_free(cb);
    BIO_free(rb);
    BIO_free(wb);
    EVP_PKEY_free(key);
    X509_free(cert);
    altdb_tls_free(t);
    return NULL;
}

int altdb_tls_add_ca(altdb_tls *t, const char *hex) {
    X509_NAME *name = X509_NAME_new();
    if (!name)
        return 0;
    int ok = X509_NAME_add_entry_by_txt(name, "O", MBSTRING_ASC, (const uint8_t *)"AdbKey-0", -1,
                                        -1, 0) &&
             X509_NAME_add_entry_by_txt(name, "CN", MBSTRING_ASC, (const uint8_t *)hex, -1, -1, 0);
    STACK_OF(X509_NAME) *names = SSL_get_client_CA_list(t->ssl);
    if (!names) {
        names = sk_X509_NAME_new_null();
        SSL_set_client_CA_list(t->ssl, names);
    }
    if (!ok || !names || !sk_X509_NAME_push(names, name)) {
        X509_NAME_free(name);
        return 0;
    }
    return 1;
}
int altdb_tls_handshake(altdb_tls *t) {
    return SSL_do_handshake(t->ssl);
}
int altdb_tls_error(altdb_tls *t, int n) {
    return SSL_get_error(t->ssl, n);
}
int altdb_tls_read(altdb_tls *t, uint8_t *p, int n) {
    return SSL_read(t->ssl, p, n);
}
int altdb_tls_write(altdb_tls *t, const uint8_t *p, int n) {
    return SSL_write(t->ssl, p, n);
}
int altdb_tls_feed(altdb_tls *t, const uint8_t *p, int n) {
    return BIO_write(SSL_get_rbio(t->ssl), p, n);
}
int altdb_tls_drain(altdb_tls *t, uint8_t *p, int n) {
    return BIO_read(SSL_get_wbio(t->ssl), p, n);
}
int altdb_tls_export(altdb_tls *t, uint8_t out[64]) {
    static const char label[] = "adb-label";
    return SSL_export_keying_material(t->ssl, out, 64, label, sizeof(label), NULL, 0, 0);
}
void altdb_tls_peer(altdb_tls *t, uint8_t out[32]) {
    memcpy(out, t->peer, 32);
}

typedef struct {
    SPAKE2_CTX *spake;
    EVP_AEAD_CTX *aead;
    uint64_t enc, dec;
} altdb_pake;
void altdb_pake_free(altdb_pake *p) {
    if (p) {
        SPAKE2_CTX_free(p->spake);
        EVP_AEAD_CTX_free(p->aead);
        OPENSSL_clear_free(p, sizeof(*p));
    }
}
altdb_pake *altdb_pake_new(int server, const uint8_t *secret, size_t len, uint8_t *msg,
                           size_t *ml) {
    static const uint8_t client[] = "adb pair client", device[] = "adb pair server";
    altdb_pake *p = OPENSSL_zalloc(sizeof(*p));
    if (!p)
        return NULL;
    p->spake = SPAKE2_CTX_new(server ? spake2_role_bob : spake2_role_alice,
                              server ? device : client, server ? sizeof(device) : sizeof(client),
                              server ? client : device, server ? sizeof(client) : sizeof(device));
    if (!p->spake || !SPAKE2_generate_msg(p->spake, msg, ml, SPAKE2_MAX_MSG_SIZE, secret, len)) {
        altdb_pake_free(p);
        return NULL;
    }
    return p;
}
int altdb_pake_finish(altdb_pake *p, const uint8_t *msg, size_t len) {
    if (!p->spake || p->aead)
        return 0;
    uint8_t material[SPAKE2_MAX_KEY_SIZE], key[16];
    size_t ml = 0;
    int ok = SPAKE2_process_msg(p->spake, material, &ml, sizeof(material), msg, len);
    SPAKE2_CTX_free(p->spake);
    p->spake = NULL;
    static const uint8_t info[] = "adb pairing_auth aes-128-gcm key";
    if (ok)
        ok = HKDF(key, sizeof(key), EVP_sha256(), material, ml, NULL, 0, info, sizeof(info) - 1);
    if (ok)
        p->aead =
            EVP_AEAD_CTX_new(EVP_aead_aes_128_gcm(), key, sizeof(key), EVP_AEAD_DEFAULT_TAG_LENGTH);
    OPENSSL_cleanse(material, sizeof(material));
    OPENSSL_cleanse(key, sizeof(key));
    return ok && p->aead;
}
int altdb_pake_crypt(altdb_pake *p, int encrypt, const uint8_t *in, size_t len, uint8_t *out,
                     size_t *ol, size_t capacity) {
    if (!p->aead)
        return 0;
    uint8_t nonce[12] = {0};
    uint64_t seq = encrypt ? p->enc : p->dec;
    if (seq == UINT64_MAX)
        return 0;
    for (size_t i = 0; i < 8; i++)
        nonce[i] = (uint8_t)(seq >> (8 * i));
    int ok = encrypt ? EVP_AEAD_CTX_seal(p->aead, out, ol, capacity, nonce, 12, in, len, NULL, 0)
                     : EVP_AEAD_CTX_open(p->aead, out, ol, capacity, nonce, 12, in, len, NULL, 0);
    if (ok) {
        if (encrypt)
            p->enc++;
        else
            p->dec++;
    }
    return ok;
}

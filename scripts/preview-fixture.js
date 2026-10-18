const fixture = {
    version: '0.1.3',
    state: 'running',
    reason: '',
    root: false,
    connections: 1,
    config: {
        enabled: true,
        port_mode: 'random',
        fixed_port: 5555,
        mdns_enabled: true,
        allow_adb_root: false,
    },
    endpoints: ['192.168.31.24:37219'],
    pairing: null,
    hosts: [
        {
            name: 'Workstation <img src=x onerror=alert(1)>',
            fingerprint: 'a1b2c3d4'.repeat(8),
            paired_at: 1789254000,
            last_connected: 1789254400,
        },
    ],
};
const scenario = new URL(location.href).searchParams.get('scenario');
if (scenario === 'paused') {
    fixture.state = 'paused_native';
    fixture.reason = 'Native USB debugging is enabled';
    fixture.endpoints = [];
}
window.ksu = {
    exec(command, options, callback) {
        const match = command.match(
            /^\/data\/adb\/modules\/altdb\/bin\/altdb ctl --request '([\w-]+)'$/,
        );
        if (!match) throw new Error('Unexpected fixture command');
        const request = JSON.parse(atob(match[1].replaceAll('-', '+').replaceAll('_', '/')));
        if (scenario === 'offline') {
            setTimeout(() => window[callback](1, '', 'Service unavailable'), 30);
            return;
        }
        let data = fixture;
        if (request.op === 'pair_start')
            fixture.pairing = {
                code: '123456',
                expires_at: Math.floor(Date.now() / 1000) + 300,
                failures: 0,
                endpoints: ['192.168.31.24:43981'],
            };
        if (request.op === 'pair_stop') fixture.pairing = null;
        if (request.op === 'configure') {
            fixture.config = request.config;
            fixture.state = request.config.enabled ? 'running' : 'disabled';
            fixture.reason = request.config.enabled ? '' : 'Disabled in settings';
        }
        if (request.op === 'revoke')
            fixture.hosts = fixture.hosts.filter((h) => h.fingerprint !== request.fingerprint);
        if (request.op === 'logs')
            data = [
                { time: 1789254400, message: 'Paired host connected' },
                { time: 1789254000, message: 'Service started' },
                { time: 1789254000, message: 'Unknown diagnostic <script>literal text</script>' },
            ];
        setTimeout(() => window[callback](0, JSON.stringify({ ok: true, data }), ''), 30);
    },
};

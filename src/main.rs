use altdb::config::{Control, DATA_DIR};
use altdb::unix::{daemon, ksu, pty, services, worker};
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::path::Path;

fn main() {
    if let Err(e) = run() {
        eprintln!("altdb: {e}");
        if e.is::<altdb::unix::store::AlreadyRunning>() {
            std::process::exit(73);
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" {
        println!(
            "altdb {}\n  daemon\n  ctl <status|logs|pair-start|pair-stop|toggle|stop>\n  ctl --request <base64url-json>\n  doctor",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if args[0] == "--version" {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let path = Path::new(DATA_DIR);
    match args[0].as_str() {
        "daemon" => {
            ensure!(args.len() == 1, "unexpected daemon options");
            daemon::run(path, false)
        }
        "worker" => worker::run(),
        "service" => services::run(),
        "pty" => pty::run(),
        "probe-ksu" => ksu::prevent_escalation(),
        "probe-shell-su" => {
            ensure!(args.len() == 2, "probe mode required");
            ksu::probe_shell_su(args[1] == "allow")
        }
        "doctor" => {
            if args.get(1).map(String::as_str) == Some("--verify-su-isolation") {
                ensure!(args.len() == 2, "unexpected doctor options");
                ksu::check()?;
                let result = ksu::verify_su_isolation();
                println!(
                    "{}",
                    match &result {
                        Ok(()) => serde_json::json!({
                            "ok": true,
                            "shell_authorized": true,
                            "ksu_no_new_privs_blocks_su": true,
                        }),
                        Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
                    }
                );
                return result;
            }
            ensure!(args.len() == 1, "unexpected doctor options");
            let result = ksu::check();
            match result {
                Ok(info) => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": true,
                            "kernelsu": info,
                            "android_min": 30,
                            "arch": "aarch64",
                        })
                    );
                    Ok(())
                }
                Err(e) => {
                    println!(
                        "{}",
                        serde_json::json!({"ok": false, "error": e.to_string()})
                    );
                    bail!("environment check failed")
                }
            }
        }
        "ctl" => {
            let request = if args.get(1).map(String::as_str) == Some("--request") {
                ensure!(
                    args.len() == 3 && args[2].len() <= 22000,
                    "invalid control payload"
                );
                serde_json::from_slice::<Control>(&URL_SAFE_NO_PAD.decode(&args[2])?)?
            } else {
                ensure!(args.len() == 2, "one control operation required");
                match args[1].as_str() {
                    "status" => Control::Status,
                    "logs" => Control::Logs,
                    "pair-start" => Control::PairStart,
                    "pair-stop" => Control::PairStop,
                    "toggle" => Control::Toggle,
                    "stop" => Control::Stop,
                    _ => bail!("unsupported control operation"),
                }
            };
            let reply = daemon::control(path, &request)?;
            if args[1] == "toggle" && reply["ok"].as_bool() == Some(true) {
                let enabled = reply["data"]["config"]["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("invalid toggle response"))?;
                println!("altdb {}", if enabled { "enabled" } else { "disabled" });
            } else {
                println!("{reply}");
            }
            ensure!(
                reply["ok"].as_bool() == Some(true),
                "control operation failed"
            );
            Ok(())
        }
        #[cfg(not(target_os = "android"))]
        "test-daemon" => {
            ensure!(args.len() == 2, "test state path required");
            daemon::run(Path::new(&args[1]), true)
        }
        #[cfg(not(target_os = "android"))]
        "test-ctl" => {
            ensure!(args.len() == 3, "test state path and JSON required");
            let request: Control = serde_json::from_str(&args[2])?;
            println!("{}", daemon::control(Path::new(&args[1]), &request)?);
            Ok(())
        }
        _ => bail!("unsupported operation"),
    }
}

#[test]
fn iso_one_shot() {
    std::env::set_var("XXSSHG_LOCAL_CMD", "cmd.exe");
    std::env::set_var("XXSSHG_LOCAL_ARGS", "/c echo ISO_HELLO");
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut handle = local::spawn(&rt.handle().clone(), local::ShellKind::Cmd, 80, 24).unwrap();
    let mut all = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        rt.block_on(async {
            tokio::select! {
                out = handle.output_rx.recv() => {
                    if let Some(b) = out {
                        all.extend_from_slice(&b);
                        // answer ConPTY's startup CPR handshake
                        if b == b"[6n" || b.ends_with(b"[6n") {
                            let _ = handle.input_tx.send(b"[1;1R".to_vec());
                            println!("answered CPR");
                        }
                    }
                }
                ev = handle.event_rx.recv() => {
                    match ev {
                        Some(e) => println!("event: {:?}", e),
                        None => println!("event channel closed"),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        });
        if all.windows(9).any(|w| w == b"ISO_HELLO") { break; }
    }
    println!("captured: {}", String::from_utf8_lossy(&all));
    assert!(all.windows(9).any(|w| w == b"ISO_HELLO"), "one-shot output missing");
}

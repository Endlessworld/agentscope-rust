//! ACP stdio driver to reproduce git timeout through the real agent.
use std::process::{Stdio, Command};
use std::io::{Write, Read};
use std::time::Duration;
use std::sync::mpsc;

fn send_msg(child: &mut std::process::Child, v: serde_json::Value) {
    let stdin = child.stdin.as_mut().unwrap();
    let s = serde_json::to_string(&v).unwrap();
    writeln!(stdin, "{s}").unwrap();
    stdin.flush().unwrap();
}

fn main() {
    let mut child = Command::new("target\\debug\\examples\\acp_harness_agent.exe")
        .current_dir(r"E:\local-github\agentscope-rust")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");

    send_msg(&mut child, serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":1,"clientCapabilities":{}}
    }));

    let stdout = child.stdout.take().unwrap();
    let (tx0, rx) = mpsc::channel::<String>();
    let tx = tx0.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        let mut acc: Vec<u8> = Vec::new();
        let mut out = stdout;
        loop {
            match out.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = acc.drain(..=pos).collect();
                        let s = String::from_utf8_lossy(&line).to_string();
                        if !s.trim().is_empty() {
                            println!("RECV {}", &s[..s.len().min(400)]);
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                                if let Some(sid) = v["result"]["sessionId"].as_str() {
                                    let _ = tx0.send(sid.to_string());
                                }
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Send session/new after 2s via helper thread; wait for sessionId (60s).
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        let _ = tx2.send("__SEND_NEW_SESSION__".to_string());
    });
    let mut sent_new = false;
    let sid = loop {
        let msg = rx.recv_timeout(Duration::from_secs(60))
            .expect("no sessionId received");
        if msg == "__SEND_NEW_SESSION__" {
            if sent_new { continue; }
            sent_new = true;
            send_msg(&mut child, serde_json::json!({
                "jsonrpc":"2.0","id":2,"method":"session/new",
                "params":{"cwd": r"E:\local-github\agentscope-rust","mcpServers":[]}
            }));
            continue;
        }
        break msg;
    };
    println!("GOT SESSION: {sid}");

    // Switch to yolo mode so tool calls skip permission review.
    send_msg(&mut child, serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"session/set_mode",
        "params":{"sessionId": sid, "modeId":"yolo"}
    }));

    send_msg(&mut child, serde_json::json!({
        "jsonrpc":"2.0","id":4,"method":"session/prompt",
        "params":{"sessionId": sid, "prompt":[{"type":"text","text":"Use the execute tool to run: git status --short  Then reply with the output."}]}
    }));

    // Wait up to 150s for the prompt result.
    let deadline = std::time::Instant::now() + Duration::from_secs(150);
    loop {
        // Drain pending permission replies and send them to the agent.
        while let Ok(msg) = rx.try_recv() {
            if msg.starts_with('{') {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
                    send_msg(&mut child, v);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
        if std::time::Instant::now() > deadline {
            println!("TIMEOUT-WAIT: no prompt result in 150s");
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
    }
    let _ = child.kill();
    let _ = reader.join();
}

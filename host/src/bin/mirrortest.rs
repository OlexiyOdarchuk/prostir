//! Checks that the physical display can be mirrored, stopped and mirrored again
//! (KWin needs a fresh connection each time): prostir-mirrortest [OUTPUT]
#[path = "../virtual_output.rs"]
#[allow(dead_code)]
mod virtual_output;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let sc = virtual_output::Screencast::connect()?;
    let name = std::env::args()
        .nth(1)
        .or_else(|| sc.physical_output())
        .expect("no physical output");
    println!("physical output: {name}");
    for round in 1..=3 {
        let t = std::time::Instant::now();
        let fresh = virtual_output::Screencast::connect()?;
        match fresh.mirror(101, &name) {
            Ok(m) => {
                println!("round {round}: node {} after {:?}", m.node, t.elapsed());
                std::thread::sleep(std::time::Duration::from_millis(500));
                drop(m);
                drop(fresh);
            }
            Err(e) => println!("round {round}: FAILED after {:?}: {e:#}", t.elapsed()),
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Ok(())
}

# nxt

USB & Bluetooth driver for communicating with the NXT brick. See [examples](https://github.com/bricks-rs/nxt/examples)
for sample code.

## Example
```rust,no_run
use nxt::{motor::*, *};

const POWER: i8 = 80;

#[tokio::main]
async fn main() -> nxt::Result<()> {
    let nxt = Nxt::first_usb().await?;
    let _nxt2 = nxt::Bluetooth::wait_for_nxt().await?;

    println!("Running motor A at {POWER}");
    nxt.set_output_state(
        OutPort::A,
        POWER,
        OutMode::ON | OutMode::REGULATED,
        RegulationMode::Speed,
        0,
        RunState::Running,
        RUN_FOREVER,
    ).await?;

    std::thread::sleep(std::time::Duration::from_secs(5));

    println!("Stop");
    nxt.set_output_state(
        OutPort::A,
        0,
        OutMode::IDLE,
        RegulationMode::default(),
        0,
        RunState::Running,
        RUN_FOREVER,
    ).await?;

    let bat = nxt.get_battery_level().await?;
    println!("Battery level is {bat} mV");

    Ok(())
}
```

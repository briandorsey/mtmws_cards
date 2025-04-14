
Example:

```text
cargo build --release --features=audio_16mb

picotool uf2 convert target/thumbv6m-none-eabi/release/backyard_rain -t elf releases/backyard_rain_16M_0_2_0.uf2
```

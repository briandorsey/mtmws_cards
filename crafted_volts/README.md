
# Crafted Volts 
*CV, get it? :)*

A utility card for the WS Computer to manually set control voltages (CV) with the input knobs and switch. In the future it will also attenuvert incomming voltages. 

```text
Audio output 1: Main knob position mapped to voltage, about -6v to +6v
Audio output 2: Main knob (inverted) position mapped to voltage, about -6v to +6v
CV output 1   : X knob posision mapped to voltage, about -6v to +6v
CV output 2   : Y knob posision mapped to voltage, about -6v to +6v
Pulse output 1: Z switch off = 0v, momentary or on = ~6v
Pulse output 2: Z switch off = ~6v, momentary or on = 0v

The six LEDs represent the state (voltage) of the output in the same location in the 2x3 grid of LEDs compared to the 2x3 grid of output jacks. LEDs are off at -6v and get brighter as the voltage goes up.
```

Note: I'm not sure if/how useful this card will be, I wrote it primarily as an exercise to learn the Computer hardware and to make sure it's all controllable from Rust.

## Releasing

TOOD: details of using elf2uf2-rs

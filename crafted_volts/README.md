
# Crafted Volts 
*CV, get it? :)*

A utility card for the WS Computer to manually set control voltages (CV)
with the input knobs and switch. It also attenuverts (attenuates and inverts)
incoming voltages.

```text
Audio output 1: Main knob position mapped to voltage, about -6v to +6v
Audio output 2: Main knob (inverted) position mapped to voltage, about -6v to +6v
Audio inputs (if any) are mixed together replacing the knob's voltage at the outputs and are attenuverted based on knob position. Output 2 is an inverted copy of output 1.. 
(these jacks are labeled "audio", but this card only samples and smooths incoming voltages expecting slow moving CV rates, consider these as additional CV inputs)

CV output 1   : X knob posision mapped to voltage, about -6v to +6v
CV output 2   : Y knob posision mapped to voltage, about -6v to +6v
CV inputs (if any) replace the knob's voltage at the outputs and are attenuverted based on knob position. 

Pulse output 1: Z switch off = 0v, momentary or on = ~6v
Pulse output 2: Z switch off = ~6v, momentary or on = 0v

The six LEDs represent the state (voltage) of the output in the same location in the 2x3 grid of LEDs compared to the 2x3 grid of output jacks. LEDs are off at -6v and get brighter as the voltage goes up.
```

Note: I'm not sure if/how useful this card will be, I wrote it primarily as an
exercise to learn the Computer hardware and to make sure it's all controllable
from Rust. Also, I'm new to both the Computer hardware and using Embassy for
embedded Rust, I expect there are many possible improvements and welcome any
recommendations.

Note: This card is sampling all inputs in a main loop. Currently the loop is
running about 275 times a second which is great for CV, but far too slow for
audio. Don't try to work from this code base to process audio signals.

## Releasing

TOOD: details of using elf2uf2-rs

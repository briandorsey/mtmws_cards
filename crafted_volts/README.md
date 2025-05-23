
# Crafted Volts 
*CV, get it? :)*

A utility card for the WS Computer to manually set control voltages (CV)
with the input knobs and switch. It also attenuverts (attenuates and inverts)
incoming voltages.

## Downloads

* [Crafted Volts firmware](https://github.com/briandorsey/mtmws_cards/releases/download/v0.2.0/crafted_volts_0_2_0.uf2)

## Installation

Download the firmware above. Then follow the "How do I write a blank program card?" instructions from the [Computer and Program Card Guide](https://www.musicthing.co.uk/Computer_Program_Cards/). 

## Documentation

<img src="CV_quickref.png" width="210px">

```text
Audio output 1: Main knob position mapped to voltage, about -6v to +6v
Audio output 2: Main knob (inverted) position mapped to voltage,
                about -6v to +6v
Audio inputs (if any) are mixed together replacing the knob's voltage at the
outputs and are attenuverted based on knob position. Output 2 is an inverted
copy of output 1. (These jacks are labeled "audio", but this card samples
and smooths incoming voltages expecting slow moving CV rates. Use these as CV
inputs)

CV output 1   : X knob posision mapped to voltage, about -6v to +6v
CV output 2   : Y knob posision mapped to voltage, about -6v to +6v
CV inputs (if any) replace the knob's voltage at the outputs and are
attenuverted based on knob position.

Pulse output 1: Z switch off = 0v, momentary or on = ~6v
Pulse output 2: Z switch off = ~6v, momentary or on = 0v

The six LEDs represent the state (voltage) of the output in the same location in
the 2x3 grid of LEDs compared to the 2x3 grid of output jacks. LEDs are off at
-6v and get brighter as the voltage goes up.
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

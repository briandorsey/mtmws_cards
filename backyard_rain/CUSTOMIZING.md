# Customizing Backyard Rain

Author: [MJLMills](https://github.com/MJLMills)

The use of WAV chunk parsing instead of using a fixed offset or raw 
samples opens the door to the replacement of the original audio files in
Backyard Rain. This allows customization with other field recordings.

### Install Rust

The first prerequisite is the installation of Rust, for which it is 
recommended the official [Rust documentation](https://www.rust-lang.org/) 
be consulted.

### Prepare Audio Files

The three files to be played back by the module need to be prepared in 
advance of editing the program source code. The files should be exported
as single-channel ADPCM WAV files with a sample rate of 48 kHz. The 
loop lengths need not be exact, but their total file size is limited by
the capacity of the program card. For Backyard Rain, the following lengths
are used.

| Card Size  | Light | Medium | Heavy |
|:-----------|-------|--------|-------|
| 2 MB       | 0m18s | 0m44s  | 0m19s |
| 16 MB      | 3m14s | 5m08s  | 2m47s |


Exporting to this format is possible with (among many other programs) 
Cockos Reaper, which is available for free trial. The three WAV files
should be placed in `backyard_rain/data` before compiling the program.

### Clone the Source Code Repo

The compile the card, the source code is required, and should be cloned
via GitHub (or optionally downloaded from the repo's main page). 

`git clone https://github.com/briandorsey/mtmws_cards`

The module source is contained in `backyard_rain/src/main.rs` which can be altered 
directly using any appropriate text editor. The lines to be changed are those
that specify the paths to (and sizes of) the three audio files.

2MB:

* backyard_rain_light_loop_short.wav
* backyard_rain_medium_loop_short.wav
* backyard_rain_heavy_loop_short.wav

16MB:

* backyard_rain_light_loop.wav
* backyard_rain_medium_loop.wav
* backyard_rain_heavy_loop.wav

The filename and size are specified in lines with the following format:

`pub const AUDIO_LIGHT: &[u8; 461844] = include_bytes!("../data/backyard_rain_light_loop_short.wav");`

These lines reserve the exact number of bytes and include the WAV file in the firmware image.
The number with value 461844 in this example must be replaced with the size of
the file in bytes. This should be available by inspecting the file's properties 
via a right-click. The filename should be replaced with the name of each custom
WAV file placed in `backyard_rain/data` in the earlier audio file preparation step.

### Compile the Card

Once the source code has been edited with the paths and sizes of the three
WAV files, the program can be compiled using the standard Rust tools.

For 2MB:

`cargo build --release --features=audio_2mb`

For 16MB:

`cargo build --release --features=audio_16mb`

The final step uses [picotool](https://github.com/raspberrypi/picotool) 
to convert the compiled card to .uf2, which needs to be installed or compiled separately.

For 2MB:

`picotool uf2 convert target/thumbv6m-none-eabi/release/backyard_rain -t elf releases/backyard_rain_2M_0_0_0.uf2`

For 16 MB:

`picotool uf2 convert target/thumbv6m-none-eabi/release/backyard_rain -t elf releases/backyard_rain_16M_0_0_0.uf2`

The custom .uf2 file will then be available in the `releases` directory at the root 
of the Backyard Rain repo.

### Transfer to the Computer Card

To update a computer card with your custom Backyard Rain program,
follow the instructions under "How do I write a blank program card?"
at the [Workshop System homepage](https://www.musicthing.co.uk/Computer_Program_Cards/).
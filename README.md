# leaneq

leaneq is an equalizer program that's lightweight

## download
[![Latest Release](https://img.shields.io/github/v/release/hartelex/leaneq?display_name=tag)](https://github.com/hartelex/leaneq/releases/latest)

it's in releases buddy

## audio routing

leaneq processes audio from the selected input device and sends the equalized output to the selected output device. wasapi process loopback is disabled because it adds a delayed copy instead of replacing the original playback.

to equalize windows system playback without doubling it, route playback through a virtual audio cable, pick its capture endpoint from the input device menu, and choose your speakers or headphones as the output.

## presets

bundled presets include flat, bass boost, vocal clarity, podcast voice, gaming focus, warm listening, and treble tame. they're starting points, so tweak them to suit your headphones and ears. preset files live in the `presets` folder beside the executable.

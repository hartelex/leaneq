# leaneq

leaneq is an equalizer program that's lightweight

## download

it's in releases buddy

## audio routing

leaneq processes audio from the selected input device and sends the equalized output to the selected output device. WASAPI process loopback is disabled because it adds a delayed copy of audio without replacing the original playback.

To equalize Windows system playback without doubling it, route playback through a virtual audio cable, select its capture endpoint from the Input device menu, and choose your speakers or headphones as the output.

## Presets

Bundled presets include Flat, Bass Boost, Vocal Clarity, Podcast Voice, Gaming Focus, Warm Listening, and Treble Tame. They are starting points; adjust them to your headphones and preferences. Preset files are stored in the `presets` folder beside the executable.

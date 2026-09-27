## Soundcheck

The point of this is to have a fast sound check tool to quickly check your sound files, get full pectral analysis and whatnot in order to get a sense of what you have in your files, what their primary characteristics are, and make it extremely fast and useable.

It was inspired by https://github.com/mrkva/sound-explorer/ - but rewritten in Rust, and made multiple useability improvements for myself and a friend of mine. Functionally it is a completely separate program, but it is very much inspired by this, but since I really... really don't like Javascript, then instead of forking and modifying, and having to use Javascript when I don't absolutely have to, a rewrite in Rust seemed like a better idea, because why not.

Easiest way to install it is from a terminal. On a Mac (Apple silicon) or Linux (x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/Erko-IM/soundcheck/main/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Erko-IM/soundcheck/main/install.ps1 | iex
```

Same command again updates it. It downloads the latest release and installs it: into /Applications on a Mac, into AppData\Local\soundcheck with a Start menu shortcut on Windows (Settings, Apps removes it), and into ~/.local/share/soundcheck on Linux, with an entry among your apps (deleting that folder, ~/.local/bin/soundcheck and ~/.local/share/applications/soundcheck.desktop removes it). Installed this way, macos and Windows don't complain about the app not being signed, because only what a browser or the like downloads gets marked as coming from the internet.

The installers are also on the [releases page](https://github.com/Erko-IM/soundcheck/releases): a dmg for Macs, an exe for Windows, and a deb and an AppImage for Linux.

The first time you open the dmg file after downloading, macos says it cannot verify soundcheck, this is because the app is not signed with a paid Apple developer account. It most likely never will be. Click Done, then open System Settings, go to Privacy & Security, scroll down to the message about soundcheck and click Open Anyway. After that it opens like any other app. 

From a terminal, this does the same if you don't want to bother with UI:

`xattr -dr com.apple.quarantine /Applications/soundcheck.app`

Windows complains the same way about a downloaded exe: when it says "Windows protected your PC", click More info, then Run anyway.

Other option is to clone repo to your machine, and run "make install" from repo root from terminal. It builds soundcheck, installs it, and installs all the necessary dependencies as well, except for a few things Windows and Linux need set up first, which the top of the Makefile lists. "make dmg", "make exe" and "make linux" build the installers themselves, and on a Mac "make packages" builds all four; a Mac builds the Windows and Linux ones in a Linux container, so Docker Desktop has to be running. Check Makefile contents to see what it is actually doing, in case you are worried.

This project is open-source - no catch, nothing proprietary, just seems like a useful tool to have, and thus... if you find it useful, feel free to use, fork, do whatever you want, but you cannot fork it to make it proprietary.

Press "m" to mark a place on the soundfile, it will also immediately put you on the name place.
Press "r" to reset the selected configuration box, or "shift+r" to reset all the configs. The small circular arrow next to each slider does the same for that one.
Click in the file list, then the up and down arrows move through it, opening each file as you land on it. If the current one is playing, the next one starts playing too.
The search box above the file list looks through every recording under the folder shown, subfolders too: names, folders, and anything the metadata and tags say. All the words have to be there, words in quotes go together, and .wav finds one kind of file. "Filters" narrows it by format, by a date between two (when it was recorded, created, modified, added to its folder, or other dates its tags carry) and by length.
Right-click a folder or a recording in the file list to keep it as a shortcut at the top, or click the star to keep the folder shown. Search and Shortcuts can be switched off in the gear's menu.
The waveform under the spectrogram is the whole file: drag across it to pick the part the spectrogram shows, drag that part's box or its ends to move or size it, and click to jump there with the playhead. A stretch picked or moved there takes the playhead to its start, and if it's playing, playback carries on from there. After a drag or a click there, left and right move it along (shift for a whole view), up and down make it wider or narrower (shift doubles or halves), and "r" shows the whole file.
The gear at the top right (or cmd+,) opens the menu with the views to show and the tools, like Pitch shift and Slow speeds.
"Slider" next to the speed buttons swaps them for a slider from 1/1000x to 1000x; click its number to type any speed, like 3.7 or 1/250.
"Bulk rename" in the gear's menu renames all the files in the explorer's folder at once, with an Insert for text or a counting number; "All rules" next to it opens a window with every rule, laid out like Bulk Rename Utility. Click the file name at the top to rename just that file.
The yellow button next to each meter mutes that channel (it turns red); with "Both speakers" ticked, the channels left play in both speakers.
Shift-drag on the spectrogram picks an area (a stretch of time and a band of frequencies) and plays just that; a new one takes its place. With "Multiple areas" ticked in the gear's menu, each new one adds to the rest, and they play in turn, together where they overlap. Shift-click one to drop it; a plain drag drops them all. "Area boxes" in the gear's menu sets how they look: edge colour and width, the dark rim and the fill.
The frequency Max stops at half the file's sample rate, the highest a recording can hold (a 48 kHz file has nothing above 24 kHz). Type a higher number, up to 1 MHz, and the part past the file's limit shows hatched: handy for seeing that nothing is there, or for keeping one scale across files recorded at different rates.
"Export PNG" saves the spectrogram as it shows, with its axes, next to the recording: at least 4096 by 2048 pixels, more for a long stretch or a large FFT, with the lettering sized to match.
The FFT menu lists how long a stretch each size works from and how close together the frequencies it tells apart are, for the file open. Larger sizes draw steady tones finer but blur what changes quickly; smaller ones keep trills, fast calls and clicks sharp.
The Metadata view edits every tag a file has: in WAV files Broadcast WAV, RIFF INFO, iXML, ID3 and GUANO (bat detectors write that one), in MP3 ID3v2, ID3v1 and APE, in FLAC and Ogg Vorbis comments, in M4A the iTunes ones and in AIFF ID3v2 and its text chunks. Each field can be changed or taken out with the ×, and "Add a field" adds one.
The rest should be pretty self-explanatory.

## License

This project is licensed under the GNU General Public License v3.0 (GPL-3.0) - see the [LICENSE](LICENSE) file for details.

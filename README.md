## Soundcheck

The point of this is to have a fast sound check tool to quickly check your sound files, get full pectral analysis and whatnot in order to get a sense of what you have in your files, what their primary characteristics are, and make it extremely fast and useable.

It was inspired by https://github.com/mrkva/sound-explorer/tree/main - but rewritten in Rust, and made multiple useability improvements for myself and a friend of mine. Functionally it is a completely separate program, but it is very much inspired by this, but since I really... really don't like Javascript, then instead of forking and modifying, and having to use Javascript when I don't absolutely have to, a rewrite in Rust seemed like a better idea, because why not.

Makefile contains commands to generate your own installer for macos - only ARM based. But repo root also contains install file itself. 

The first time you open the dmg file after downloading, macos says it cannot verify soundcheck, this is because the app is not signed with a paid Apple developer account. It most likely never will be. Click Done, then open System Settings, go to Privacy & Security, scroll down to the message about soundcheck and click Open Anyway. After that it opens like any other app. 
From a terminal, this does the same: `xattr -dr com.apple.quarantine /Applications/soundcheck.app` if you don't want to bother with UI.
Other option is to clone repo to your machine, and run "make dmg" from repo root from terminal. It will install all the necessary dependencies on your mac as well. Check Makefile contents to see what it is actually doing, in case you are worried.

Will add the same for Linux and Windows later on.

This project is open-source - no catch, nothing proprietary, just seems like a useful tool to have, and thus... if you find it useful, feel free to use, fork, do whatever you want, but you cannot fork it to make it proprietary.

Press "m" to mark a place on the soundfile, it will also immediately put you on the name place.
Press "r" to reset the selected configuration box, or "shift+r" to reset all the configs.
The rest should be pretty self-explanatory.

## License

This project is licensed under the GNU General Public License v3.0 (GPL-3.0) - see the [LICENSE](LICENSE) file for details.

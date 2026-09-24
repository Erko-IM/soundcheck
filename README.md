The point of this is to have a fast sound check tool to quickly check your sound files, get full pectral analysis and whatnot in order to get a sense of what you have in your files, what their primary characteristics are, and make it extremely fast and useable.

It was inspired by https://github.com/mrkva/sound-explorer/tree/main - but rewritten in Rust, and made multiple useability improvements for myself and a friend of mine. Functionally it is a completely separate program, but it is very much inspired by this, but since I really... really don't like Javascript, then instead of forking and modifying, and having to suffer through JS, a rewrite in Rust seemed like a better idea - strong type system, manual memory management, and various other benefits.

Makefile contains commands to generate your own installer for macos - only ARM based. But repo root also contains install file itself. 

Will add the same for Linux and Windows later on.
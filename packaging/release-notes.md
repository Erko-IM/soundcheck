Installed with one line in a terminal, soundcheck opens without the macOS and Windows warnings about apps that aren't signed. On a Mac (Apple silicon) or Linux (x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/Erko-IM/soundcheck/main/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Erko-IM/soundcheck/main/install.ps1 | iex
```

The files below work too, but macOS and Windows warn about them once when they're downloaded with a browser. The [README](https://github.com/Erko-IM/soundcheck#readme) says how to get past that.

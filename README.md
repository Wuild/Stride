# Stride

A Linux terminal prototype built with Rust, GTK 4, and libadwaita. Stride owns the terminal grid, scrollback, screen modes, selection, command navigation, and keyboard routing. It uses Linux PTYs for shell I/O and the Rust `vte` crate to parse terminal byte sequences.

## Install or update on Fedora and Nobara

On Fedora or Nobara 44 (x86-64), enable the signed Stride repository once:

```sh
sudo dnf config-manager addrepo --from-repofile=https://wuild.github.io/Stride/stride.repo
sudo dnf install stride
```

After that, `sudo dnf upgrade` includes new Stride versions when they are
published to the [RPM feed](https://wuild.github.io/Stride/). Its signing key
fingerprint is `15468090849900177FABB8D814F8BDD786AE6595`.

Standalone packages are also available from the
[GitHub releases](https://github.com/Wuild/Stride/releases).

### Set Stride as your default terminal

On desktops that use [xdg-terminal-exec](https://github.com/Vladimir-csp/xdg-terminal-exec),
put `io.stride.Terminal.desktop` on the first line of
`${XDG_CONFIG_HOME:-$HOME/.config}/xdg-terminals.list`. If you have a
desktop-specific list, such as `gnome-xdg-terminals.list`, add the entry there
instead so it takes precedence. Run `xdg-terminal-exec --print-id` to confirm
the selected terminal. The installer leaves your default selection up to you.

Stride can also run a command directly inside its terminal:

```sh
stride --working-directory /tmp -e printf '%s\n' 'argument with spaces'
```

### Build the RPM

On Fedora or Nobara, install the build dependencies and run:

```sh
sudo dnf install cargo rust gcc rpm-build desktop-file-utils gtk4-devel libadwaita-devel pango-devel
./packaging/build-rpm.sh ./rpmbuild
```

The binary RPM is written under `rpmbuild/RPMS/`. Build and test on the
distribution release you intend to support.

## Run

From this directory, with a recent stable Rust toolchain and a Linux graphical session and GTK 4, libadwaita 1.6 or newer, and PangoCairo development packages:

```sh
cargo run --release --locked
cargo test --locked
```

Stride starts new sessions in your home folder and launches `$SHELL`, defaulting to Bash. With Bash it loads `~/.bashrc` through a temporary startup file and adds session-local prompt and Readline hooks. It uses Bash’s existing history and leaves your dotfiles as they are.

| Input | Action |
| --- | --- |
| Drag | Select terminal text |
| Ctrl+A | Select the editable Bash command |
| Delete / Backspace | Remove the selected part of the editable command |
| Ctrl+C | Copy and clear a selection, or send interrupt if there is no selection |
| Ctrl+Shift+C | Copy selection |
| Ctrl+V | Paste at an integrated Bash prompt, or send Ctrl+V to the active program |
| Ctrl+Shift+V | Paste into the active program |
| Up / Down | Pick a suggestion when the list is open |
| Ctrl+Shift+Enter | Prepare a simple external command to run with sudo |
| Right Arrow | Accept a visible command, path, or Bash history suggestion at the end of the line |
| Ctrl+Shift+F | Open scrollback search |
| Ctrl+Shift+L | Jump to the last failed Bash command |
| Shift+Page Up / Down | Scroll by one screen |
| Ctrl+Shift+Plus / Minus | Adjust terminal font size |

Click within the editable command to move the caret. For prediction, run `printf 'hello from Stride\n'`, type just `printf 'hello` at the next prompt, and look for the muted suffix. Press **Right Arrow** to accept it and **Enter** to run it. When multiple suggestions match, the dropdown opens above the prompt. Up and Down select a match. Enter accepts a match after you navigate the list. Press Enter again to run the completed line. Escape dismisses the list. Prediction runs locally. For command names try `print`, or type `cd /usr/sh` for a directory suggestion. For arguments and options, Stride uses installed Bash completion definitions, including when a command is preceded by `sudo`. Stride also learns recurring commands and arguments from recent Bash history. It appends to Bash’s own history as commands finish, subject to your history settings. Set `STRIDE_SYNC_HISTORY=0` before launching to keep Bash’s normal history write timing. Path and argument completion currently handles simple unquoted prefixes. DNF package completion is available with Bash’s Tab key.

The Bash prompt replaces your home directory prefix with a Home icon and your username, followed by each folder below it. Outside home, the prompt shows the absolute path in separate theme-colored blocks. Click a parent block to change into it, or click the current folder to open a list of parents. Navigation keeps unfinished commands in place. Standard folder icons follow your desktop’s configured locations. Set `STRIDE_COMPACT_PROMPT=0` before launching to keep your configured Bash prompt. Open the searchable Command History sidebar from the toolbar. Live entries show exit status and can jump to or copy their output. Earlier entries come from Bash’s history file and keep the command text. Stride displays up to 300 earlier entries, 200 current-session entries, and 5,000 scrollback lines. Bash history settings determine which command labels and predictions are available.

The window and prompt follow libadwaita’s light/dark preference and system accent. GTK provides the window controls and desktop clipboard.

Commands that support Polkit can request authorization through the desktop. For other simple external commands, Ctrl+Shift+Enter prepares a sudo command for review. Enter executes the displayed command. Recognized local `sudo` prompts stay in the terminal and show inline dots while you type. Stride keeps only the display count, and sudo receives the keystrokes through its usual no-echo terminal input.

## Tabs and split panes

| Shortcut | Action |
| --- | --- |
| Ctrl+T | Open a tab |
| Ctrl+Page Up / Down | Switch tabs |
| Ctrl+Shift+E | Split side by side |
| Ctrl+Shift+O | Split top and bottom |
| Ctrl+Shift+[ / ] | Focus the previous or next pane |
| Ctrl+Shift+W | Close the focused pane, or close its tab if it is the last pane |

Press Ctrl+T to open a tab. Right-click the terminal and select Split, then Vertical or Horizontal. Right-click the terminal or use Shift+F10 for clipboard and search actions, or right-click a tab for tab and split actions. Drag a divider to resize panes. Closing a window, tab, or pane that has a running command asks for confirmation. Every pane has its own shell, scrollback, and working directory. With one tab, the header shows the window title. Titles follow the current directory and foreground program of the focused pane.

## Prototype scope

Stride highlights common Bash input tokens and recognizes JSON and log levels in plain output. It supports ANSI colors, true color, cursor movement, alternate screens, and bracketed paste. Terminal compatibility is partial. Scrollback does not reflow after resizing, mouse reporting for terminal apps is absent, and font fallback is limited. Command navigation works in Bash and depends on its prompt hooks. Apps that redraw in place or produce output after a command has finished can make the recorded output range approximate.

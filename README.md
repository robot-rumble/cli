# Rumblebot (Robot Rumble CLI tool)

### Features

 1. Easily run robot programs stored on your filesystem, which makes it possible to use a custom IDE.
 2. View battle results in both a web GUI (like on the site), or directly in your terminal.
 3. Easily create and update robots from the command line.

### Installation

 1. Download the appropriate release from the Github releases page.
 2. Unzip the folder somewhere in your filesystem.
 3. Open a terminal and navigate to the folder. If you've never used a terminal before, you can read a quick primer [here](https://lifehacker.com/a-command-line-primer-for-beginners-5633909) (it's way easier than it seems!) In this case, you will want to run `cd [FOLDER_PATH]`.
 4. On windows, you can go ahead and run the program with `./rumblebot.exe`. On Macos/Linux, you first need to make the `rumblebot` file runnable by executing `chmod +x ./rumblebot`. Then you can run it with `./rumblebot`.

**Please note**: Just because your robots live in the filesystem does not mean that you can import external dependencies. Support for third-party packages is on roadmap, but implementing it is a very nuanced challenge. If you really, really need a library, your best bet is to minify it and just include it at the top of your program.

Vanillite @VERSION@ for Minecraft Java Edition @MC@
=====================================================

A Minecraft Java Edition server written in Rust that behaves like Vanilla in game.
This is a beta: back up your worlds and report problems at
https://github.com/hthoene/vanillite/issues

Start
-----
Java is not needed. Put the program into its own folder and start it from there:

    @START@

The server keeps its files (configuration, worlds, cache) in the current folder;
`--dir <folder>` picks another one. `@START@ --help` lists all commands,
`@START@ --version` prints the version.

1. The first start creates eula.txt and asks whether you accept the Minecraft
   EULA (https://aka.ms/MinecraftEULA): type yes and press Enter, and it saves
   eula=true and starts. Without a console to answer in, it stops instead: set
   eula=true in eula.txt and start again (or set the environment variable
   VANILLITE_EULA=true).
2. The first real start downloads Mojang's server jar (for the Vanilla data) into
   cache/, so it needs internet access once, and generates the spawn area.
3. When the log shows "Done", connect with Minecraft Java Edition @MC@ to
   localhost (port 25565). Change the port with server-port in server.properties.

Stop the server with the console command `stop` or Ctrl+C: both save the worlds.

Paper and Spigot plugins: put the plugin jars into plugins/ and start the server.
It installs what they need (the Bukkit layer and, if missing, a Java 25 runtime)
into plugins/bukkit/ and says every step in the log.

Settings: server.properties as in Vanilla, plus vanillite.toml. Every key can also
be set as an environment variable VANILLITE_<KEY>, e.g. VANILLITE_SERVER_PORT=25566.

Windows:
Windows SmartScreen may warn the first time: choose More info > Run anyway. A
double click starts it in its own folder.

macOS:
The program is not signed. The first time, open it with a right click (Control-click)
on vanillite > Open, and confirm. Or allow it in Terminal once:
    xattr -d com.apple.quarantine vanillite
Then start it from Terminal in its folder as shown above.

Licensed under the Apache License 2.0 (LICENSE, NOTICE). Not an official Minecraft
product; not approved by or associated with Mojang or Microsoft.

TiddlyDesktop v0.0.37
========================

A desktop application for TiddlyWiki.

REQUIREMENTS
------------
- webkit2gtk (webkit2gtk-4.1 or webkit2gtk-4.0)
- libayatana-appindicator3 (or libappindicator-gtk3)
- GTK 3

On Slackware, install:
  sbopkg -i webkit2gtk libappindicator

On other distros, the package names may vary.

INSTALLATION
------------
Option 1: System-wide (as root)
  ./install.sh

Option 2: Custom prefix
  ./install.sh /opt/tiddlydesktop

Option 3: Run directly without installing
  ./bin/tiddlydesktop-rs

UNINSTALLATION
--------------
  ./uninstall.sh

or with custom prefix:
  ./uninstall.sh /opt/tiddlydesktop


# Translations

RFiles uses gettext with the text domain `rfiles`. The English text in the source is the message id, so with no
catalogue installed everything stays English.

Only some strings are wrapped so far: the right-click menus (`item_menu`, `background_menu` in `src/app.rs`),
the result banner (`src/banner.rs`) and the trash, delete and restore messages (`src/trash.rs`). The rest of
the app follows the same pattern and is converted file by file.

## In code

```rust
use crate::i18n::{ntr, tr};

tr("Open in Terminal")                                   // a plain string
ntr(n, "Moved {n} item to the trash", "Moved {n} items to the trash")   // singular, plural; {n} becomes the count
```

Don't build a sentence from translated pieces; translate whole sentences, with `{n}` for numbers.

## Extract the template

From the repository root:

```sh
xgettext --from-code=UTF-8 -L C --keyword=tr --keyword=ntr:2,3 --add-comments \
    --package-name=rfiles -f po/POTFILES.in -o po/rfiles.pot
```

## Add a language

```sh
msginit -i po/rfiles.pot -l de -o po/de.po --no-translator    # or copy rfiles.pot to de.po
# translate the strings in de.po and set the Plural-Forms header for the language
msgfmt -c -o po/de.mo po/de.po
mkdir -p ~/.local/share/locale/de/LC_MESSAGES
cp po/de.mo ~/.local/share/locale/de/LC_MESSAGES/rfiles.mo
```

RFiles looks in `~/.local/share/locale` first when it holds an `rfiles.mo`, otherwise in `/usr/share/locale`.
Start RFiles with `LANGUAGE=de rfiles` to try it. Send the `.po` file; `install.sh` installs every compiled
`po/<language>.mo` it finds.

use super::*;

/// `bind-key` (R25 grown up): binds in `Registry.key_tables` as a runtime
/// binding, which `list-keys` marks and `reload` keeps (R10). A single
/// command argument is a command string, split like a stored one (R12);
/// several are split at `;` arguments as the CLI splits argv (tmux 3.7b:
/// `bind y split-window '\;' select-pane -L` binds both). Each command
/// must parse, as tmux checks at bind time; no command at all changes
/// nothing, as in tmux. The binding keeps the request's dialect (spec
/// §4.5), so a key bound through the tmux front end runs through it.
pub fn bind_key(state: &mut State, ctx: &ClientCtx, c: BindKey) -> Result<String, String> {
    let key = parse_key(&c.key).map_err(|_| format!("unknown key: {}", c.key))?;
    let cmds = match c.command.as_slice() {
        [one] => split_command_string(one).map_err(|e| e.to_string())?,
        [] => return Ok(String::new()),
        argv => crate::cmd::parse::split_argv(argv),
    };
    if cmds.is_empty() {
        return Ok(String::new());
    }
    // An alias (TC.7) is expanded now, as tmux expands it when the binding
    // is made: the binding keeps what it ran then.
    let cmds: Vec<Vec<String>> = cmds
        .into_iter()
        .flat_map(|argv| {
            crate::server::dispatch::menu::expand_alias_command(state, &argv)
                .unwrap_or_else(|| vec![argv])
        })
        .collect();
    for argv in &cmds {
        crate::compat::parse_split(ctx, argv)?;
    }
    let mut b = Binding::new(cmds, c.repeat, BindingSource::Runtime);
    b.desc = c.note;
    b.group = c.group;
    b.dialect = ctx.dialect;
    state.registry.key_tables.insert(&c.table, key, b);
    Ok(String::new())
}

/// `unbind-key [-a] [-T table] key`, worded as tmux 3.7b words it: an
/// unknown key or table is an error, an unbound key is not.
pub fn unbind_key(state: &mut State, _ctx: &ClientCtx, c: UnbindKey) -> Result<String, String> {
    let tables = &mut state.registry.key_tables;
    // `-q`: a failure with nothing said, which tmux's exit status shows as
    // success.
    if c.quiet && !c.all && c.key.is_none() {
        return Ok(String::new());
    }
    let key = match &c.key {
        Some(name) => match parse_key(name) {
            Ok(key) => Some(key),
            Err(_) if c.quiet => return Ok(String::new()),
            Err(_) => return Err(format!("unknown key: {name}")),
        },
        None => None,
    };
    if !tables.exists(&c.table) {
        if c.quiet {
            return Ok(String::new());
        }
        return Err(format!("table {} doesn't exist", c.table));
    }
    let keys = match key {
        Some(key) => vec![key],
        None => tables.keys(&c.table),
    };
    for key in keys {
        tables.unbind_runtime(&c.table, &key);
    }
    Ok(String::new())
}

/// `list-keys [-1N] [-T table] [key]` in grofe's native form, one line per
/// binding: `bind-key [-r] -T <table> <key> <command>` as tmux spells it,
/// then `  # <source>`, with `: <desc>` and `[<name>]` when set. The source
/// is `default`, `<file>:<line>` (`~` for `$HOME`), `runtime` or
/// `plugin <name>`, so a binding from outside the config table shows. A
/// key lists only its binding; `-1` only the first line; `-N` the notes
/// ([`list_notes`]).
// ponytail: tmux's padded `list-keys` layout is FE.4's.
pub fn list_keys(state: &mut State, ctx: &ClientCtx, c: ListKeys) -> Result<String, String> {
    let only = match &c.key {
        Some(name) => Some(parse_key(name).map_err(|_| format!("invalid key: {name}"))?),
        None => None,
    };
    let sort = Sort::new(&c.sort)?;
    let tables = &state.registry.key_tables;
    if let Some(t) = &c.table
        && !tables.exists(t)
    {
        return Err(format!("table {t} doesn't exist"));
    }
    // The flags only the tmux listing has choose it in either dialect.
    let tmux_listing = c.format.is_some() || c.prefix.is_some() || c.all || c.sort.order.is_some();
    if ctx.dialect == crate::protocol::Dialect::Tmux || tmux_listing {
        return list_keys_tmux(state, ctx, &c, only, sort);
    }
    if c.notes {
        return list_notes(state, ctx, &c, only);
    }
    let names: Vec<&str> = match &c.table {
        Some(t) => vec![t.as_str()],
        None => tables.table_names().collect(),
    };
    let home = config_env().home;
    let mut out = String::new();
    for table in names {
        for key in tables.keys(table) {
            if only.is_some_and(|k| !k.matches(&key)) {
                continue;
            }
            let Some(b) = tables.exact(table, &key) else {
                continue;
            };
            out.push_str(&binding_line(table, &key, b, home.as_deref()));
            out.push('\n');
            if c.first {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// `list-keys -N`: each binding with a note as `<prefix><key> <note>`, keys
/// padded to the widest, as tmux 3.7b prints them (recorded: without `-T`,
/// the prefix table then the root table, every line after the prefix
/// key, `C-b `; with `-T`, that table alone and no prefix). With `-1` only
/// the first, and when a client is current it goes to that client's status
/// line instead, unpadded, as tmux's stock `prefix /` shows it.
// ponytail: tmux 3.7b's CLI prints nothing when a key is given, with or
// without `-N` or `-1` (recorded: `list-keys -T prefix c`, `list-keys -1
// -T prefix c` and `list-keys -N c` all print nothing), and `-1N` of a key
// without a note shows the first noted binding; grofe lists what the key
// names. Matching tmux's output is FE.4's.
pub(super) fn list_notes(
    state: &mut State,
    ctx: &ClientCtx,
    c: &ListKeys,
    only: Option<Key>,
) -> Result<String, String> {
    let tables = &state.registry.key_tables;
    let prefix = match state.model.options.lookup("prefix", &[]) {
        Some(p) if !p.eq_ignore_ascii_case("None") => format!("{p} "),
        _ => String::new(),
    };
    let names: Vec<(&str, &str)> = match &c.table {
        Some(t) => vec![(t.as_str(), "")],
        None => vec![("prefix", prefix.as_str()), ("root", prefix.as_str())],
    };
    let mut notes: Vec<(String, String)> = Vec::new();
    for (table, start) in names {
        for key in tables.keys(table) {
            if only.is_some_and(|k| !k.matches(&key)) {
                continue;
            }
            let Some(b) = tables.exact(table, &key) else {
                continue;
            };
            let Some(note) = b.desc.as_deref().filter(|n| !n.is_empty()) else {
                continue;
            };
            let group = b
                .group
                .as_deref()
                .map_or(String::new(), |g| format!(" [{g}]"));
            notes.push((format!("{start}{key}{group}"), note.to_string()));
        }
    }
    if c.first {
        notes.truncate(1);
        if let (Some((key, note)), Some(id)) = (notes.first(), clients::current_client(state, ctx))
        {
            clients::show_message_as(
                state,
                id,
                &format!("{key} {note}"),
                crate::server::render::Level::Info,
            );
            return Ok(String::new());
        }
    }
    let width = notes
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    Ok(notes
        .iter()
        .map(|(key, note)| format!("{key:<width$} {note}\n"))
        .collect())
}

/// tmux 3.7b's `LIST_KEYS_TEMPLATE`, a line of `list-keys`.
pub(super) const TMUX_LIST_KEYS: &str = "#{?notes_only,\
#{key_prefix} #{p|#{key_string_width}:key_string} #{?key_note,#{key_note},#{key_command}},\
bind-key #{?key_has_repeat,#{?key_repeat,-r,  },} \
-T #{p|#{key_table_width}:key_table} #{p|#{key_string_width}:#{q|a:key_string}} #{key_command}}";

/// The variables of one `list-keys` line (`cmd_list_keys_format_add_key_binding`).
pub(super) struct KeyVars(BTreeMap<&'static str, String>);

impl FormatVars for KeyVars {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// `list-keys` as tmux 3.7b prints it (`cmd-list-keys.c`, recorded):
///
/// - which bindings: `-T`'s table, or with `-N` the prefix then the root
///   table, or every table by name; each table in tmux's key order
///   ([`tmux_key_order`]) or the order `-O` asks for (`-r` reversing it); a
///   key given keeps only its binding, `-N` only bindings with a note
///   (`-a`: every one), `-1` only the first;
/// - a line is `-F`'s format, else tmux's template, over the line's key
///   variables: by default `bind-key [-r] -T <table> <key> <commands>`, the
///   table and the key (escaped as tmux escapes an argument) padded to the
///   widest, `-r` or two blanks only when some binding repeats; with `-N`,
///   `<prefix> <key> <note>` (`-P`'s prefix, else the `prefix` key);
/// - one binding (a key given, or `-1`) goes to the current client's
///   status line, not to the output.
///
/// The commands are tmux's stock text for a stock binding, else each one
/// printed as tmux prints a parsed command ([`tmux_command_line`]).
// ponytail: tmux compares key codes as `int`s, so its `-O index` and
// `-O modifier` order the keys it cannot tell apart by the low bits of the
// difference of their codes; here they order by the key's place in tmux's
// key table (index) or its modifiers (modifier), and `-O name` is by table.
pub(super) fn list_keys_tmux(
    state: &mut State,
    ctx: &ClientCtx,
    c: &ListKeys,
    only: Option<Key>,
    sort: Sort,
) -> Result<String, String> {
    let tables = &state.registry.key_tables;
    let names: Vec<String> = match &c.table {
        Some(t) => vec![t.clone()],
        None if c.notes => vec!["prefix".to_string(), "root".to_string()],
        None => tables.table_names().map(str::to_string).collect(),
    };
    // Each table, in key order; sorted as a whole unless a table or `-N`
    // names the tables to sort one by one.
    let mut groups: Vec<Vec<(String, Key, Binding)>> = Vec::new();
    for table in &names {
        let mut keys = tables.keys(table);
        keys.sort_by_key(tmux_key_order);
        groups.push(
            keys.into_iter()
                .filter_map(|key| Some((table.clone(), key, tables.exact(table, &key)?.clone())))
                .collect(),
        );
    }
    // tmux's `sort_key_binding_cmp` is no total order (it takes the
    // difference of two 64 bit key codes as an `int`, and a tie as "greater"
    // when the tables are the same), so its order is the one its `qsort`
    // makes of it: the same merge sort here.
    type Row = (String, Key, Binding);
    let compare = |order: Order, a: &Row, b: &Row| -> i32 {
        let same_table = a.0.eq_ignore_ascii_case(&b.0);
        let first = match order {
            Order::Index => tmux_key_low32(&a.1).wrapping_sub(tmux_key_low32(&b.1)) as i32,
            Order::Name => i32::from(same_table),
            _ => 0,
        };
        let result = if first == 0 {
            i32::from(same_table)
        } else {
            first
        };
        if sort.reversed() { -result } else { result }
    };
    let sort_rows = |rows: &mut [Row]| match sort.order() {
        None => {}
        Some(Order::Natural) => {
            if sort.reversed() {
                rows.reverse();
            }
        }
        Some(order) => {
            crate::server::dispatch::sorting::merge_sort(rows, &|a, b| compare(order, a, b))
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    if c.table.is_some() || c.notes {
        for group in &mut groups {
            sort_rows(group);
        }
        rows.extend(groups.into_iter().flatten());
    } else {
        rows.extend(groups.into_iter().flatten());
        sort_rows(&mut rows);
    }
    let filter_notes = c.notes && !c.all;
    rows.retain(|(_, key, b)| {
        only.is_none_or(|k| k.matches(key)) && (!filter_notes || b.desc.is_some())
    });
    if c.first {
        rows.truncate(1);
    }
    let prefix = match &c.prefix {
        Some(p) => p.clone(),
        None => match state.model.options.lookup("prefix", &[]) {
            Some(p) if !p.eq_ignore_ascii_case("None") => p.to_string(),
            _ => String::new(),
        },
    };
    let width = |s: &str| unicode_width::UnicodeWidthStr::width(s);
    let key_width = rows
        .iter()
        .map(|(_, k, _)| width(&k.to_string()))
        .max()
        .unwrap_or(0);
    let table_width = rows.iter().map(|(t, _, _)| width(t)).max().unwrap_or(0);
    let has_repeat = rows.iter().any(|(_, _, b)| b.repeat);
    let template = match &c.format {
        Some(f) if ctx.dialect == crate::protocol::Dialect::Tmux => tmux_template(f),
        Some(f) => f.clone(),
        None => tmux_template(TMUX_LIST_KEYS),
    };
    let flag = |on: bool| if on { "1" } else { "0" }.to_string();
    let lines: Vec<String> = rows
        .iter()
        .map(|(table, key, b)| {
            let command = b
                .shown
                .clone()
                .unwrap_or_else(|| tmux_command_line(&b.cmds));
            let vars = KeyVars(BTreeMap::from([
                ("notes_only", flag(c.notes)),
                ("key_has_repeat", flag(has_repeat)),
                ("key_string_width", key_width.to_string()),
                ("key_table_width", table_width.to_string()),
                ("key_repeat", flag(b.repeat)),
                ("key_note", b.desc.clone().unwrap_or_default()),
                ("key_prefix", prefix.clone()),
                ("key_table", table.clone()),
                ("key_string", key.to_string()),
                ("key_command", command),
            ]));
            expand(&template, &vars)
        })
        .collect();
    if lines.len() == 1 || c.first {
        if let (Some(line), Some(id)) = (lines.first(), clients::current_client(state, ctx)) {
            clients::show_message_as(state, id, line, crate::server::render::Level::Info);
        }
        return Ok(String::new());
    }
    Ok(lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| format!("{l}\n"))
        .collect())
}

/// The low 32 bits of tmux's key code for `key` (`key_code`): what is left
/// of a key code once its type and modifier bits are out of the way, and
/// all `-O index` sees of it (it compares the difference as an `int`).
// ponytail: a character of four UTF-8 bytes is an index into tmux's table of
// them, which is not reproduced; its bytes stand in.
pub(super) fn tmux_key_low32(key: &Key) -> u32 {
    use crate::server::input::{KeyCode, MousePlace};
    use unicode_width::UnicodeWidthChar;
    let packed = |c: char| {
        let mut buf = [0u8; 4];
        let bytes = c.encode_utf8(&mut buf).as_bytes();
        let width = c.width().unwrap_or(0) as u32;
        let data = bytes
            .iter()
            .take(3)
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (8 * i));
        (bytes.len() as u32) << 24 | (width + 1) << 29 | data
    };
    // Past KEYC_NONE: the functional keys, in tmux's order.
    let function = |n: u32| n;
    match key.code {
        KeyCode::Char(c) if c.is_ascii() => c as u32,
        KeyCode::Char(c) => packed(c),
        KeyCode::Literal(c) => packed(c),
        KeyCode::Control(c) => u32::from(c),
        KeyCode::Tab => 9,
        KeyCode::Enter => 13,
        KeyCode::Escape => 27,
        KeyCode::User(n) => u32::from(n),
        KeyCode::Any => function(4),
        KeyCode::Backspace => function(7),
        KeyCode::F(n) => function(7 + u32::from(n)),
        KeyCode::Insert => function(20),
        KeyCode::Delete => function(21),
        KeyCode::Home => function(22),
        KeyCode::End => function(23),
        KeyCode::PageDown => function(24),
        KeyCode::PageUp => function(25),
        KeyCode::BackTab => function(26),
        KeyCode::Up => function(27),
        KeyCode::Down => function(28),
        KeyCode::Left => function(29),
        KeyCode::Right => function(30),
        KeyCode::Keypad(k) => function(31 + k.order()),
        KeyCode::Mouse(m) => {
            let place = match m.place {
                MousePlace::Pane => 0,
                MousePlace::Status => 1,
                MousePlace::StatusLeft => 2,
                MousePlace::StatusRight => 3,
                MousePlace::StatusDefault => 4,
                MousePlace::Border => 5,
                MousePlace::ScrollbarUp => 6,
                MousePlace::ScrollbarSlider => 7,
                MousePlace::ScrollbarDown => 8,
                MousePlace::Control(n) => 9 + u32::from(n),
            };
            u32::from(m.button) << 8 | place
        }
    }
}

/// Where tmux 3.7b's key codes put `key` (its key tables are ordered by
/// code): no modifier, then Meta, Ctrl and Shift as the high bits (`M-`
/// 1, `C-` 2, `S-` 4); within one, characters by code point (Enter, Tab
/// and Escape are C0 characters), then tmux's special keys in its own
/// order, then the mouse keys.
pub(crate) fn tmux_key_order(key: &Key) -> (u8, u8, u32, String) {
    use crate::server::input::KeyCode;
    let m = &key.mods;
    let mods = u8::from(m.alt) | u8::from(m.ctrl) << 1 | u8::from(m.shift) << 2;
    const SPECIAL: &[KeyCode] = &[
        KeyCode::Backspace,
        KeyCode::Insert,
        KeyCode::Delete,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageDown,
        KeyCode::PageUp,
        KeyCode::BackTab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
    ];
    // tmux orders by the key code: the characters and C0 codes, the user
    // keys, the functional keys, then the mouse keys.
    match key.code {
        // The characters tmux packs as UTF-8 (those past ASCII, and those a
        // `0xNN` made) come after the C0 codes and ASCII, in that order.
        KeyCode::Char(c) if c.is_ascii() => (mods, 0, c as u32, String::new()),
        KeyCode::Literal(c) => (mods, 0, 0x80 + c as u32, String::new()),
        KeyCode::Char(c) => (mods, 0, 0x1000 + c as u32, String::new()),
        KeyCode::Enter => (mods, 0, 13, String::new()),
        KeyCode::Tab => (mods, 0, 9, String::new()),
        KeyCode::Escape => (mods, 0, 27, String::new()),
        KeyCode::Control(c) => (mods, 0, u32::from(c), String::new()),
        KeyCode::User(n) => (mods, 1, u32::from(n), String::new()),
        // F1 to F12 sit between BSpace and IC.
        KeyCode::F(n) => (mods, 2, u32::from(n), String::new()),
        // The keypad follows the arrows.
        KeyCode::Keypad(k) => (mods, 2, 25 + k.order(), String::new()),
        KeyCode::Mouse(m) => (mods, 3, mouse_order(m), String::new()),
        code => {
            let at = SPECIAL.iter().position(|s| *s == code).unwrap_or(0) as u32;
            let at = if at == 0 { 0 } else { at + 12 };
            (mods, 2, at, String::new())
        }
    }
}

/// Where tmux 3.7b's key codes put a mouse key: by action (`KEYC_MOUSE_KEY`
/// order, the wheel down before up, as recorded), then button, then where
/// the pointer was (pane, the status line's ranges, border, scrollbar,
/// border controls).
pub(super) fn mouse_order(m: crate::server::input::MouseKey) -> u32 {
    use crate::server::input::{MouseAction as A, MousePlace as P};
    let action = match m.action {
        A::Down => 0,
        A::Up => 1,
        A::Drag => 2,
        A::DragEnd => 3,
        A::WheelDown => 4,
        A::WheelUp => 5,
        A::SecondClick => 6,
        A::DoubleClick => 7,
        A::TripleClick => 8,
    };
    let place = match m.place {
        P::Pane => 0,
        P::Status => 1,
        P::StatusLeft => 2,
        P::StatusRight => 3,
        P::StatusDefault => 4,
        P::Border => 5,
        P::ScrollbarUp => 6,
        P::ScrollbarSlider => 7,
        P::ScrollbarDown => 8,
        P::Control(n) => 9 + u32::from(n),
    };
    action * 10_000 + u32::from(m.button) * 100 + place
}

/// A binding's commands as tmux 3.7b prints a parsed command list in
/// `list-keys` (`cmd_list_print` escaped): each command by its full tmux
/// name, its flags without values first in one word, then each flag with
/// a value, both in tmux's flag order (ASCII: digits, upper, lower case),
/// then the arguments, every value escaped as tmux escapes one; commands
/// joined by ` \; `.
pub(crate) fn tmux_command_line(cmds: &[Vec<String>]) -> String {
    cmds.iter()
        .map(|argv| tmux_command(argv))
        .collect::<Vec<_>>()
        .join(" \\; ")
}

pub(super) fn tmux_command(argv: &[String]) -> String {
    let Some((name, args)) = argv.split_first() else {
        return String::new();
    };
    let Ok(spec) = crate::compat::tmux::resolve(name) else {
        // A native-only verb: as it is.
        let mut out = vec![name.clone()];
        out.extend(args.iter().map(|a| quote::tmux_arg(a)));
        return out.join(" ");
    };
    // Flags taking a value: `[-c word]` in the usage (and `select-pane
    // -P`, which tmux hides).
    let mut takes_value: Vec<char> = spec
        .usage
        .split("[-")
        .skip(1)
        .filter_map(|part| {
            let mut chars = part.chars();
            let flag = chars.next()?;
            (chars.next() == Some(' ')).then_some(flag)
        })
        .collect();
    if spec.name == "select-pane" {
        takes_value.push('P');
    }
    let mut bools: BTreeMap<char, usize> = BTreeMap::new();
    let mut values: BTreeMap<char, Vec<String>> = BTreeMap::new();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(letters) = arg.strip_prefix('-').filter(|l| !l.is_empty()) else {
            break;
        };
        i += 1;
        for (at, flag) in letters.char_indices() {
            if !takes_value.contains(&flag) {
                *bools.entry(flag).or_default() += 1;
                continue;
            }
            let attached = &letters[at + flag.len_utf8()..];
            let value = if attached.is_empty() {
                i += 1;
                args.get(i - 1).cloned().unwrap_or_default()
            } else {
                attached.to_string()
            };
            values.entry(flag).or_default().push(value);
            break;
        }
    }
    let mut out = spec.name.to_string();
    if !bools.is_empty() {
        out.push_str(" -");
        for (flag, n) in &bools {
            out.extend(std::iter::repeat_n(*flag, *n));
        }
    }
    for (flag, vals) in &values {
        for v in vals {
            out.push_str(&format!(" -{flag} {}", quote::tmux_arg(v)));
        }
    }
    for a in &args[i.min(args.len())..] {
        out.push(' ');
        out.push_str(&quote::tmux_arg(a));
    }
    out
}

pub(super) fn binding_line(table: &str, key: &Key, b: &Binding, home: Option<&Path>) -> String {
    let repeat = if b.repeat { "-r " } else { "" };
    let cmds: Vec<String> = b
        .cmds
        .iter()
        .map(|argv| {
            argv.iter()
                .map(|a| quote::tmux_arg(a))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let source = match &b.source {
        BindingSource::Default => "default".to_string(),
        BindingSource::File { path, line } => format!("{}:{line}", tilde(path, home)),
        BindingSource::Runtime => "runtime".to_string(),
        BindingSource::Plugin(name) => format!("plugin {name}"),
    };
    let mut line = format!(
        "bind-key {repeat}-T {table} {} {}  # {source}",
        quote::tmux_arg(&key.to_string()),
        cmds.join(" \\; ")
    );
    if let Some(desc) = &b.desc {
        line.push_str(&format!(": {desc}"));
    }
    if let Some(name) = &b.name {
        line.push_str(&format!(" [{name}]"));
    }
    line
}

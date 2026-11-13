//! What a file is, for the IDE: its tree icon (the Electron app's
//! IDEFileIcons: a coloured glyph) and the language to highlight it with.

use std::path::Path;

/// (glyph, colour) for a file in the tree and tab bar.
pub fn icon(path: &Path) -> (&'static str, u32) {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    match name.as_str() {
        ".gitignore" | ".gitmodules" | ".gitattributes" => return ("g", 0xefb080),
        "dockerfile" => return ("dk", 0x87c3ff),
        "makefile" => return ("mk", 0x6e7781),
        "license" => return ("-", 0x6e7781),
        "readme.md" => return ("v", 0x75d3ba),
        _ => {}
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "ts" => ("{}", 0x87c3ff),
        "tsx" => ("<>", 0x87c3ff),
        "js" | "json" => ("{}", 0xf0c674),
        "jsx" => ("<>", 0xf0c674),
        "md" => ("v", 0x75d3ba),
        "html" | "xml" => ("<>", 0xefb080),
        "css" => ("#", 0x87c3ff),
        "scss" | "less" => ("#", 0xaaa0fa),
        "yaml" | "yml" => ("!", 0xe567dc),
        "py" => ("py", 0x87c3ff),
        "rs" => ("rs", 0xefb080),
        "go" => ("go", 0x75d3ba),
        "java" => ("j", 0xefb080),
        "rb" => ("rb", 0xe567dc),
        "sh" | "bash" | "zsh" => ("$", 0xa8cc7c),
        "sql" => ("db", 0xf0c674),
        "swift" => ("sw", 0xefb080),
        "c" => ("c", 0x87c3ff),
        "cpp" => ("c+", 0x87c3ff),
        "h" => ("h", 0x87c3ff),
        "hpp" => ("h+", 0x87c3ff),
        "php" => ("php", 0xaaa0fa),
        "svg" => ("svg", 0xf0c674),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" => ("img", 0xaaa0fa),
        "toml" | "ini" | "env" | "editorconfig" => ("{}", 0x6e7781),
        "txt" | "log" => ("-", 0x6e7781),
        "lock" => ("*", 0x6e7781),
        _ => (".", 0x6e7781),
    }
}

/// (highlighter name, display name) — the Electron app's detectLanguage,
/// mapped onto the tree-sitter grammars this build has ("text" when none).
pub fn language(path: &Path) -> (&'static str, &'static str) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.split('.').next().unwrap_or("");
    match stem {
        "Dockerfile" => return ("text", "Dockerfile"),
        "Makefile" => return ("make", "Makefile"),
        "Rakefile" | "Gemfile" | "Vagrantfile" => return ("ruby", "Ruby"),
        "CMakeLists" => return ("cmake", "CMake"),
        _ => {}
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "ts" => ("typescript", "Typescript"),
        "tsx" => ("tsx", "Typescript"),
        "js" | "jsx" | "mjs" | "cjs" => ("javascript", "Javascript"),
        "json" | "lock" => ("json", "Json"),
        "md" => ("markdown", "Markdown"),
        "html" | "vue" => ("html", "Html"),
        "svelte" => ("svelte", "Html"),
        "css" | "scss" | "less" => ("css", "Css"),
        "xml" => ("html", "Xml"),
        "yaml" | "yml" => ("yaml", "Yaml"),
        "toml" => ("toml", "Toml"),
        "ini" | "env" | "editorconfig" => ("text", "Ini"),
        "proto" => ("proto", "Protobuf"),
        "graphql" => ("graphql", "Graphql"),
        "py" => ("python", "Python"),
        "rb" => ("ruby", "Ruby"),
        "rs" => ("rust", "Rust"),
        "go" => ("go", "Go"),
        "java" => ("java", "Java"),
        "kt" => ("kotlin", "Kotlin"),
        "swift" => ("swift", "Swift"),
        "c" | "h" => ("c", "C"),
        "cpp" | "hpp" | "cc" => ("cpp", "Cpp"),
        "cs" => ("csharp", "Csharp"),
        "php" => ("php", "Php"),
        "sql" => ("sql", "Sql"),
        "lua" => ("lua", "Lua"),
        "sh" | "bash" | "zsh" | "fish" => ("bash", "Shell"),
        "zig" => ("zig", "Zig"),
        "ex" | "exs" => ("elixir", "Elixir"),
        "scala" => ("scala", "Scala"),
        _ => ("text", "Plaintext"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_by_name_then_extension() {
        assert_eq!(icon(Path::new("/x/.gitignore")), ("g", 0xefb080));
        assert_eq!(icon(Path::new("/x/README.md")), ("v", 0x75d3ba));
        assert_eq!(icon(Path::new("/x/main.rs")), ("rs", 0xefb080));
        assert_eq!(icon(Path::new("/x/App.TSX")), ("<>", 0x87c3ff));
        assert_eq!(icon(Path::new("/x/weird.zzz")), (".", 0x6e7781));
    }

    #[test]
    fn languages() {
        assert_eq!(language(Path::new("a/main.rs")), ("rust", "Rust"));
        assert_eq!(language(Path::new("a/App.tsx")), ("tsx", "Typescript"));
        assert_eq!(language(Path::new("a/Makefile")), ("make", "Makefile"));
        assert_eq!(language(Path::new("a/notes.txt")), ("text", "Plaintext"));
    }
}

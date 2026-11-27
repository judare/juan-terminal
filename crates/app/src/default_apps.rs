//! Make Fragua the default app for Office documents, PDFs, Markdown, plain text, source
//! code, images, video and audio. Done once per VERSION (marker file), only from the installed
//! .app, so a later choice in Finder ("Open With → Change All") wins.

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSSetDefaultRoleHandlerForContentType(content_type: CFStringRef, role: u32, handler: CFStringRef) -> i32;
    fn UTTypeCreatePreferredIdentifierForTag(tag_class: CFStringRef, tag: CFStringRef, conforming_to: CFStringRef) -> CFStringRef;
}

const BUNDLE_ID: &str = "dev.juan-terminal.app";
/// Bump to apply again (e.g. when new types are added).
const VERSION: &str = "v3";

/// Content types claimed directly.
pub const TYPES: &[&str] = &[
    "com.adobe.pdf",
    "org.openxmlformats.wordprocessingml.document",
    "org.openxmlformats.wordprocessingml.document.macroenabled",
    "org.openxmlformats.spreadsheetml.sheet",
    "org.openxmlformats.spreadsheetml.sheet.macroenabled",
    "org.openxmlformats.presentationml.presentation",
    "org.openxmlformats.presentationml.presentation.macroenabled",
    "net.daringfireball.markdown",
    "public.plain-text",
    "public.source-code",
    "public.json",
    "public.yaml",
    "public.shell-script",
    "public.python-script",
    "public.ruby-script",
    "public.perl-script",
    "public.php-script",
    "public.c-source",
    "public.c-header",
    "public.c-plus-plus-source",
    "public.c-plus-plus-header",
    "public.objective-c-source",
    "public.swift-source",
    "com.sun.java-source",
    "com.netscape.javascript-source",
    "public.css",
    "public.log",
    "public.image",
    "public.png",
    "public.jpeg",
    "org.webmproject.webp",
    "com.compuserve.gif",
    "public.tiff",
    "com.microsoft.bmp",
    "com.microsoft.ico",
    "public.heic",
    "public.heif",
    "public.avif",
    "public.movie",
    "public.mpeg-4",
    "com.apple.quicktime-movie",
    "com.apple.m4v-video",
    "public.3gpp",
    "public.audio",
    "public.mp3",
    "com.microsoft.waveform-audio",
    "com.apple.m4a-audio",
    "public.aac-audio",
    "org.xiph.flac",
    "public.aiff-audio",
    "com.apple.coreaudio-format",
];

/// Extensions resolved to whatever UTI the system maps them to (declared by another app or a
/// `dyn.*` one), so files like .rs/.ts/.vue/.toml open here too.
const EXTENSIONS: &[&str] = &[
    "md", "markdown", "mdx", "txt", "rs", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "vue", "svelte", "json", "jsonc", "json5", "toml",
    "yaml", "yml", "sh", "bash", "zsh", "fish", "py", "rb", "go", "java", "kt", "kts", "swift", "c", "h", "cpp", "hpp", "cc", "cs", "php", "sql",
    "css", "scss", "sass", "less", "lua", "ini", "conf", "cfg", "env", "log", "lock", "gitignore", "dockerignore", "editorconfig", "graphql", "gql",
    "prisma", "proto", "tf", "dart", "ex", "exs", "zig", "nix", "gradle", "xml", "plist",
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "avif", "heic", "heif", "tga",
    "mp4", "mov", "m4v", "webm", "avi", "mkv", "3gp", "mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "aiff", "aif", "caf",
];

fn uti_for_extension(ext: &str) -> Option<String> {
    let class = CFString::new("public.filename-extension");
    let tag = CFString::new(ext);
    let r = unsafe { UTTypeCreatePreferredIdentifierForTag(class.as_concrete_TypeRef(), tag.as_concrete_TypeRef(), std::ptr::null()) };
    (!r.is_null()).then(|| unsafe { CFString::wrap_under_create_rule(r) }.to_string())
}

pub fn claim_once() {
    let in_bundle = std::env::current_exe().ok().is_some_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
    if !in_bundle {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = std::path::Path::new(&home).join("Library/Application Support/juan-terminal");
    let marker = dir.join(format!("default-apps-{VERSION}"));
    if marker.exists() {
        return;
    }
    let mut types: Vec<String> = TYPES.iter().map(|s| s.to_string()).collect();
    for ext in EXTENSIONS {
        if let Some(u) = uti_for_extension(ext) {
            if !types.contains(&u) {
                types.push(u);
            }
        }
    }
    let handler = CFString::new(BUNDLE_ID);
    let mut failed = Vec::new();
    for t in &types {
        let ct = CFString::new(t);
        // kLSRolesAll
        let status = unsafe { LSSetDefaultRoleHandlerForContentType(ct.as_concrete_TypeRef(), 0xFFFF_FFFF, handler.as_concrete_TypeRef()) };
        if status != 0 {
            failed.push(format!("{t}: {status}"));
        }
    }
    // Some system types refuse a handler change; don't retry forever over those.
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&marker, format!("{}\n\nfailed:\n{}", types.join("\n"), failed.join("\n")));
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolves_extensions() {
        assert!(super::uti_for_extension("md").is_some());
        assert!(super::uti_for_extension("rs").is_some());
    }
}

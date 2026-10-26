#[path = "src/skillmeta.rs"]
mod skillmeta;

fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("../assets/故事工坊.ico");
    // 发布形态（frozen）：入门功能卡图在编译期内嵌，生成表由 http.rs include!。
    // 文件名口径与 http::guide_name_ok 一致，表外名字照旧 404。
    if std::env::var_os("CARGO_FEATURE_FROZEN").is_some() {
        embed_guide(&manifest_dir);
        embed_skills(&manifest_dir);
    }
    let attributes = tauri_build::Attributes::new()
        .app_manifest(
            tauri_build::AppManifest::new()
                .commands(&["pick_folder", "shell_close_answer", "shell_quit"])
                .permissions_path_pattern("permissions/manual/*"),
        )
        .windows_attributes(tauri_build::WindowsAttributes::new().window_icon_path(&icon));
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}

fn embed_guide(manifest_dir: &std::path::Path) {
    let dir = manifest_dir.join("../assets/guide");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if guide_name_ok(&name) {
                entries.push((name, entry.path()));
            }
        }
    }
    entries.sort();
    if entries.is_empty() {
        panic!("frozen 构建找不到入门功能卡图：{}", dir.display());
    }
    let mut table = String::from(
        "// build.rs 生成：入门功能卡图内嵌表（frozen 形态专用）。\n\
         pub(crate) static GUIDE_ASSETS: &[(&str, &[u8])] = &[\n",
    );
    for (name, path) in &entries {
        table.push_str(&format!("    ({name:?}, include_bytes!({path:?})),\n"));
    }
    table.push_str("];\n");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = std::path::Path::new(&out_dir).join("guide_assets.rs");
    std::fs::write(&dest, table).expect("写 guide_assets.rs 失败");
}

fn guide_name_ok(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("guide-") else {
        return false;
    };
    let Some((base, ext)) = rest.rsplit_once('.') else {
        return false;
    };
    if base.is_empty() || !base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return false;
    }
    matches!(ext, "webp" | "png" | "jpg" | "jpeg")
}

/// 发布形态（frozen）：内置技能在编译期内嵌，生成表由 http.rs include!。
/// 与 embed_guide 不同：目录缺失/为空生成空表而非 panic——技能库可清空、
/// 验证 demo 可整目录摘除（ADR 0020）。目录名口径与 http::skill_dir_ok 一致。
/// L2：manifest 升级元数据数组——编译期预解析 frontmatter＋同权重预算 tokens（skillmeta），
/// 前端列表/过滤/触发判定只读元数据、不拉正文；SKILL_FILES 仍内嵌正文供勾选后按需取。
/// L3：manifest 每技能附 `refs`（references/ 合法参考文件名清单，纯元数据）；SKILL_REFS
/// 内嵌参考文件字节（`references/<文件>` 相对路径），供助理 skill-load 的 ref 按需读取。
fn embed_skills(manifest_dir: &std::path::Path) {
    let dir = manifest_dir.join("../skills");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if skill_dir_ok(&name) && entry.path().join("SKILL.md").is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    let mut manifest = String::from(
        "// build.rs 生成：内置技能内嵌表（frozen 形态专用；空表合法，ADR 0020）。\n\
         // SKILL_MANIFEST＝元数据 JSON 数组（L2：dir/name/description/cats/triggers/tokens；L3：附 refs 清单）。\n\
         pub(crate) static SKILL_MANIFEST: &str = ",
    );
    let mut entries: Vec<String> = Vec::new();
    let mut files = String::from("pub(crate) static SKILL_FILES: &[(&str, &[u8])] = &[\n");
    let mut refs_table = String::from(
        "pub(crate) static SKILL_REFS: &[(&str, &str, &[u8])] = &[\n",
    );
    for name in &names {
        let path = dir.join(name).join("SKILL.md");
        let text = std::fs::read(&path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let refs = list_ref_files(&dir.join(name).join("references"));
        entries.push(skillmeta::meta_json(name, &text, &refs));
        files.push_str(&format!("    ({name:?}, include_bytes!({path:?})),\n"));
        for ref_name in &refs {
            let ref_path = dir.join(name).join("references").join(ref_name);
            let rel = format!("references/{ref_name}");
            refs_table.push_str(&format!("    ({name:?}, {rel:?}, include_bytes!({ref_path:?})),\n"));
        }
    }
    manifest.push_str(&format!("{:?};\n", format!("[{}]", entries.join(","))));
    files.push_str("];\n");
    refs_table.push_str("];\n");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = std::path::Path::new(&out_dir).join("skills_assets.rs");
    std::fs::write(&dest, format!("{manifest}{files}{refs_table}")).expect("写 skills_assets.rs 失败");
}

/// 列技能 references/ 子目录的合法参考文件名（skillmeta::ref_file_ok 白名单；排序稳定）。
fn list_ref_files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if skillmeta::ref_file_ok(&name) && entry.path().is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

fn skill_dir_ok(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

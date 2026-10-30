//! 字体资产的回归测试。
//!
//! 这里不复用 GPUI 的 TextSystem，而是直接用 fontdb（GPUI 在 Linux 上
//! 底层的同一套实现，版本锁在 0.16.x）验证：四个 .ttf 能被解析成
//! **同一个家族**的四档字重。
//!
//! 这条断言值得单独测，因为 MiSans 发布时把每个字重做成了独立家族
//! （"MiSans" / "MiSans Medium" / "MiSans Semibold"），若子集化脚本的
//! name 表改写失效，`font_weight(SEMIBOLD)` 会静默退化成 Regular ——
//! 界面上表现为"标题不加粗"，肉眼很难在代码评审时发现。

const REGULAR: &[u8] = include_bytes!("../assets/fonts/MiSans-Regular.ttf");
const MEDIUM: &[u8] = include_bytes!("../assets/fonts/MiSans-Medium.ttf");
const SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/MiSans-Semibold.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/MiSans-Bold.ttf");

fn db() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    for f in [REGULAR, MEDIUM, SEMIBOLD, BOLD] {
        db.load_font_data(f.to_vec());
    }
    db
}

fn query(db: &fontdb::Database, w: fontdb::Weight) -> fontdb::ID {
    db.query(&fontdb::Query {
        families: &[fontdb::Family::Name("MiSans")],
        weight: w,
        ..Default::default()
    })
    .unwrap_or_else(|| panic!("weight {w:?} 查询 MiSans 失败"))
}

#[test]
fn four_weights_share_one_family() {
    let db = db();
    assert_eq!(db.len(), 4, "应加载 4 个字重");

    for id in db.faces().map(|f| f.id).collect::<Vec<_>>() {
        let face = db.face(id).unwrap();
        assert!(
            face.families.iter().any(|(name, _)| name == "MiSans"),
            "字重 {:?} 的家族名不是 MiSans: {:?}",
            face.post_script_name,
            face.families
        );
    }
}

#[test]
fn weight_query_resolves_to_distinct_faces() {
    let db = db();
    let mut seen = Vec::new();
    for w in [
        fontdb::Weight::NORMAL,
        fontdb::Weight::MEDIUM,
        fontdb::Weight::SEMIBOLD,
        fontdb::Weight::BOLD,
    ] {
        let id = query(&db, w);
        let face = db.face(id).unwrap();
        assert_eq!(face.weight, w, "weight {w:?} 命中了错误的字重");
        seen.push(id);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 4, "四个字重查询到了重复的 face");
}

#[test]
fn required_glyphs_present() {
    let db = db();
    let id = query(&db, fontdb::Weight::NORMAL);
    db.with_face_data(id, |data, index| {
        let face = ttf_parser::Face::parse(data, index).expect("ttf-parser 解析失败");
        for ch in "AcgDev下载任务暂停恢复删除设置速度剩余时间连接数分片保存"
            .chars()
            .chain("，。、；：「」（）《》—…“”■□●○▲▼★☆✓→↑↓".chars())
            .chain("0123456789.%/¥·".chars())
        {
            assert!(
                face.glyph_index(ch).is_some(),
                "缺少字形: {ch} U+{:04X}",
                ch as u32
            );
        }
    })
    .expect("读取字体数据失败");
}

#[test]
fn copyright_notice_retained() {
    // MiSans 许可协议第 2 条第 4 款：副本须保留版权声明
    let db = db();
    let id = query(&db, fontdb::Weight::NORMAL);
    db.with_face_data(id, |data, index| {
        let face = ttf_parser::Face::parse(data, index).unwrap();
        let name = face
            .names()
            .into_iter()
            .find(|n| n.name_id == ttf_parser::name_id::COPYRIGHT_NOTICE)
            .and_then(|n| n.to_string())
            .unwrap_or_default();
        assert!(
            name.to_lowercase().contains("xiaomi"),
            "版权声明丢失或被改写: {name:?}"
        );
    })
    .expect("读取字体数据失败");
}

#[test]
fn font_bytes_are_subset_sized() {
    // 防止误把未子集化的完整字体（每个 ~8 MB）提交进来
    for (name, f) in [
        ("Regular", REGULAR),
        ("Medium", MEDIUM),
        ("Semibold", SEMIBOLD),
        ("Bold", BOLD),
    ] {
        let mb = f.len() as f64 / 1048576.0;
        assert!(
            (1.0..3.0).contains(&mb),
            "{name} 体积 {mb:.2} MB 超出子集化预期区间 1–3 MB"
        );
    }
}


#[test]
fn debug_seed_34_detail() {
    let p = pair(34);
    let (engine, ids) = ingest_pair(&p);
    let outcome = engine
        .merge_tree_changes(Some(ids[p.base_index]), ids[p.ours_index], ids[p.theirs_index])
        .unwrap();
    let changes = outcome.unwrap();
    for c in &changes {
        if c.path.contains("note_7") {
            println!("change: {:?}", c);
        }
    }
    for label in ["base", "ours", "theirs"] {
        let i = match label {
            "base" => p.base_index,
            "ours" => p.ours_index,
            _ => p.theirs_index,
        };
        let files: Vec<String> = engine
            .flatten_tree(engine.derive_tree(ids[i]).unwrap())
            .unwrap()
            .into_iter()
            .map(|(p, _, _)| p)
            .filter(|p| p.contains("note_7") || p == "sub/note_7" || p.contains("sub/note"))
            .collect();
        println!("{label}: {files:?}");
    }
}

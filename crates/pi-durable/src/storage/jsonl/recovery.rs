use super::*;
use crate::env::{FileErrorCode, FileKind};
struct ParsedLine<T> {
    value: T,
    start: u64,
}
struct ParsedFile<T> {
    path: PathBuf,
    lines: Vec<ParsedLine<T>>,
}
type Key = (String, Seq, u64);
fn key(file: &str, seq: Seq, ordinal: u64) -> Key {
    (file.into(), seq, ordinal)
}
impl JsonlStorage {
    pub(super) async fn recover(&self, context: &Context) -> Result<()> {
        let fs = &self.inner.fs;
        let main = read_lines(fs, &self.inner.main_path, MAIN_FILE, context, parse_marker).await?;
        let mut previous = Seq::from_raw(0);
        for marker in &main.lines {
            if marker.value.seq <= previous {
                return Err(corrupt(format!(
                    "Commit sequence does not strictly increase in {MAIN_FILE}"
                )));
            }
            previous = marker.value.seq;
        }
        let listing = fs
            .list_dir(&self.inner.directory, context)
            .await
            .map_err(|e| Error::jsonl_io("directory listing", e))?;
        for file in &listing {
            if file.kind == FileKind::File
                && file
                    .name
                    .to_str()
                    .and_then(|n| n.strip_suffix(RECLAIM_SUFFIX))
                    .is_some_and(is_sidecar_file)
            {
                let _ = fs.remove(&file.path, false, true, context).await;
            }
        }
        let mut names = listing
            .iter()
            .filter(|f| f.kind == FileKind::File)
            .filter_map(|f| f.name.to_str())
            .filter(|n| is_sidecar_file(n))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        names.sort();
        let mut parsed_files = IndexMap::new();
        let mut records: HashMap<Key, SidecarRecord> = HashMap::new();
        for name in names {
            let path = self.resolve_file(&name, context).await?;
            let parsed = read_lines(fs, &path, &name, context, |text, line| {
                parse_record(text, &name, line)
            })
            .await?;
            let mut previous = None;
            for line in &parsed.lines {
                let position = (line.value.seq, line.value.ordinal);
                if previous.is_some_and(|old| position <= old) {
                    return Err(corrupt(format!(
                        "Sidecar records are out of order in {name}"
                    )));
                }
                previous = Some(position);
                records.insert(
                    key(&name, line.value.seq, line.value.ordinal),
                    line.value.clone(),
                );
            }
            parsed_files.insert(name, parsed);
        }
        let mut current_only = HashSet::new();
        let mut retired = HashSet::new();
        let mut final_task_live = IndexMap::new();
        for marker in &main.lines {
            for operation in &marker.value.writes {
                match operation {
                    MainOperation::Create { record, .. } if record.semantics.current_only() => {
                        current_only.insert(record.id);
                    }
                    MainOperation::Direct(StorageWrite::DocumentRetire { id }) => {
                        retired.insert(*id);
                    }
                    MainOperation::Direct(StorageWrite::Task { value }) => {
                        final_task_live.insert(value.id(), false);
                    }
                    MainOperation::Task { id, .. } => {
                        final_task_live.insert(*id, true);
                    }
                    _ => {}
                }
            }
        }
        let retired_current: HashSet<_> = retired.intersection(&current_only).copied().collect();
        let mut latest_bases: HashMap<DocumentId, (Seq, u64)> = HashMap::new();
        for marker in &main.lines {
            for operation in &marker.value.writes {
                let (id, ordinal) = match operation {
                    MainOperation::Create { record, ordinal } => (record.id, *ordinal),
                    MainOperation::Change { id, ordinal } => (*id, *ordinal),
                    _ => continue,
                };
                if !current_only.contains(&id) {
                    continue;
                }
                let record =
                    records.get(&key(&file_name("doc", id.get()), marker.value.seq, ordinal));
                if record.is_some_and(|r|matches!(&r.payload,SidecarPayload::Document {id:record_id,content:DocumentContent::Base {..}} if *record_id==id)){
                let position=(marker.value.seq,ordinal);let base=latest_bases.entry(id).or_insert(position);if position>*base{*base=position;}
            }
            }
        }
        let before_base = |id: DocumentId, seq: Seq, ordinal: u64| {
            latest_bases
                .get(&id)
                .is_some_and(|base| (seq, ordinal) < *base)
        };
        let terminal_tasks: HashSet<_> = final_task_live
            .iter()
            .filter(|(_, live)| !**live)
            .map(|(id, _)| *id)
            .collect();
        let mut confirmed = HashSet::new();
        for line in &main.lines {
            let marker = &line.value;
            let mut writes = Vec::new();
            for operation in &marker.writes {
                match operation {
                    MainOperation::Direct(write) => writes.push(write.clone()),
                    MainOperation::Task { id, ordinal } => {
                        let optional = terminal_tasks.contains(id);
                        let record = confirm(
                            marker,
                            *ordinal,
                            &file_name("task", id.get()),
                            &records,
                            &mut confirmed,
                            optional,
                        )?;
                        if let Some(record) = record {
                            let SidecarPayload::Task(task) = &record.payload else {
                                return Err(corrupt(format!(
                                    "Confirmed task sidecar data does not match commit {}",
                                    marker.seq
                                )));
                            };
                            if task.id() != *id {
                                return Err(corrupt(format!(
                                    "Confirmed task sidecar data does not match commit {}",
                                    marker.seq
                                )));
                            }
                            if !optional {
                                writes.push(StorageWrite::Task {
                                    value: task.clone(),
                                });
                            }
                        }
                    }
                    MainOperation::Create { ordinal, .. }
                    | MainOperation::Change { ordinal, .. } => {
                        let id = match operation {
                            MainOperation::Create { record, .. } => record.id,
                            MainOperation::Change { id, .. } => *id,
                            _ => unreachable!(),
                        };
                        let reclaimed =
                            retired_current.contains(&id) || before_base(id, marker.seq, *ordinal);
                        let sidecar = confirm(
                            marker,
                            *ordinal,
                            &file_name("doc", id.get()),
                            &records,
                            &mut confirmed,
                            reclaimed,
                        )?;
                        let content = if let Some(sidecar) = sidecar {
                            let SidecarPayload::Document {
                                id: record_id,
                                content,
                            } = &sidecar.payload
                            else {
                                return Err(corrupt(format!(
                                    "Confirmed document sidecar data does not match commit {}",
                                    marker.seq
                                )));
                            };
                            if *record_id != id {
                                return Err(corrupt(format!(
                                    "Confirmed document sidecar data does not match commit {}",
                                    marker.seq
                                )));
                            }
                            Some(content.clone())
                        } else {
                            None
                        };
                        match operation {
                            MainOperation::Create { record, .. } => {
                                if matches!(content, Some(DocumentContent::Delta { .. })) {
                                    return Err(corrupt(format!(
                                        "Document creation lacks a confirmed base in commit {}",
                                        marker.seq
                                    )));
                                }
                                let content = match content {
                                    Some(DocumentContent::Base { version, value })
                                        if !reclaimed =>
                                    {
                                        DocumentBase { version, value }
                                    }
                                    _ => DocumentBase {
                                        version: 1,
                                        value: JsonObject::default(),
                                    },
                                };
                                writes.push(StorageWrite::DocumentCreate {
                                    record: record.clone(),
                                    content,
                                });
                            }
                            MainOperation::Change { .. } => {
                                if !reclaimed && let Some(content) = content {
                                    writes.push(StorageWrite::DocumentChange { id, content });
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            self.inner
                .memory
                .prepare_commit(&writes, Some(marker.seq))
                .and_then(|p| p.apply())
                .map_err(|e| {
                    Error::jsonl_corruption(
                        format!("Invalid committed state at sequence {}", marker.seq),
                        Some(e),
                    )
                })?;
        }
        let mut reclamations = IndexMap::new();
        for (file, parsed) in &parsed_files {
            let mut unconfirmed_at = None;
            for line in &parsed.lines {
                if confirmed.contains(&key(file, line.value.seq, line.value.ordinal)) {
                    if unconfirmed_at.is_some() {
                        return Err(corrupt(format!(
                            "Confirmed record follows an unconfirmed tail in {file}"
                        )));
                    }
                } else if unconfirmed_at.is_none() {
                    unconfirmed_at = Some(line.start);
                }
            }
            if let Some(at) = unconfirmed_at {
                fs.truncate_file(&parsed.path, at, context)
                    .await
                    .map_err(|e| Error::jsonl_io(format!("tail truncation of {file}"), e))?;
            }
            let confirmed_lines = parsed
                .lines
                .iter()
                .filter(|l| confirmed.contains(&key(file, l.value.seq, l.value.ordinal)))
                .collect::<Vec<_>>();
            let mut retained = None;
            if let Some((kind, numeric_id)) = sidecar_file(file) {
                if kind == "task"
                    && numeric_id <= i64::MAX as u64
                    && terminal_tasks.contains(&TaskId::from_raw(numeric_id as i64))
                {
                    retained = Some(Vec::new());
                } else if kind == "doc" && numeric_id <= i64::MAX as u64 {
                    let id = DocumentId::from_raw(numeric_id as i64);
                    if retired_current.contains(&id) {
                        retained = Some(Vec::new());
                    } else if latest_bases.contains_key(&id) {
                        retained = Some(
                            confirmed_lines
                                .iter()
                                .copied()
                                .filter(|l| !before_base(id, l.value.seq, l.value.ordinal))
                                .collect(),
                        );
                    }
                }
            }
            if let Some(retained) = retained
                && (retained.len() < confirmed_lines.len() || retained.is_empty())
            {
                reclamations.insert(
                    file.clone(),
                    retained.iter().map(|l| l.value.line()).collect(),
                );
            }
        }
        self.reclaim_sidecars(reclamations, context).await;
        let mut metadata = self.inner.metadata.lock();
        metadata.current_only.extend(current_only);
        metadata.live_tasks.extend(
            final_task_live
                .into_iter()
                .filter(|(_, live)| *live)
                .map(|(id, _)| id),
        );
        Ok(())
    }
}
fn confirm<'a>(
    marker: &MainMarker,
    ordinal: u64,
    file: &str,
    records: &'a HashMap<Key, SidecarRecord>,
    confirmed: &mut HashSet<Key>,
    optional: bool,
) -> Result<Option<&'a SidecarRecord>> {
    let key = key(file, marker.seq, ordinal);
    if confirmed.contains(&key) {
        return Err(corrupt("Sidecar record is confirmed more than once"));
    }
    let Some(record) = records.get(&key) else {
        return if optional {
            Ok(None)
        } else {
            Err(corrupt(format!(
                "Missing confirmed sidecar record {file} at sequence {}",
                marker.seq
            )))
        };
    };
    confirmed.insert(key);
    Ok(Some(record))
}
async fn read_lines<T>(
    fs: &Arc<dyn FileSystem>,
    path: &Path,
    name: &str,
    context: &Context,
    parse: impl Fn(&str, usize) -> Result<T>,
) -> Result<ParsedFile<T>> {
    let bytes = match fs.read_binary_file(path, context).await {
        Ok(bytes) => bytes,
        Err(error) if error.code() == FileErrorCode::NotFound => {
            return Ok(ParsedFile {
                path: path.into(),
                lines: Vec::new(),
            });
        }
        Err(error) => return Err(Error::jsonl_io(format!("read of {name}"), error)),
    };
    let complete = if bytes.last().is_some_and(|b| *b != b'\n') {
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
        fs.truncate_file(path, complete as u64, context)
            .await
            .map_err(|e| Error::jsonl_io(format!("torn-line truncation of {name}"), e))?;
        complete
    } else {
        bytes.len()
    };
    let mut lines = Vec::new();
    let mut start = 0;
    for end in 0..complete {
        if bytes[end] != b'\n' {
            continue;
        }
        let number = lines.len() + 1;
        let text = std::str::from_utf8(&bytes[start..end]).map_err(|e| {
            Error::jsonl_corruption(
                format!("Invalid UTF-8 in complete {name} line {number}"),
                Some(Error::message(e.to_string())),
            )
        })?;
        let value = parse(text.strip_prefix('\u{feff}').unwrap_or(text), number)?;
        lines.push(ParsedLine {
            value,
            start: start as u64,
        });
        start = end + 1;
    }
    Ok(ParsedFile {
        path: path.into(),
        lines,
    })
}

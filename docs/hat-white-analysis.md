root = Path('/opt/nanobot-rust/nanobot-rust')
crates_dir = root / 'crates'
crate_paths = sorted([p for p in crates_dir.iterdir() if p.is_dir() and
(p/'Cargo.toml').exists()])
crate_names = [p.name for p in crate_paths]
rs_files = sorted(root.glob('crates/**/*.rs'))
# ---------- helpers ----------
def read_text(path):
    return path.read_text(encoding='utf-8')
def rel(path):
    return path.relative_to(root).as_posix()
def line_no_from_offset(text, offset):
    return text.count('\n', 0, offset) + 1
def line_offsets(text):
    offs = [0]
    for m in re.finditer('\n', text):
        offs.append(m.end())
    return offs
def line_at(text, line_no):
    lines = text.splitlines()
    return lines[line_no-1] if 1 <= line_no <= len(lines) else ''
def strip_inline_comments(s):
    # crude but enough for signature formatting
    s = re.sub(r'//.*', '', s)
    return s
def collect_signature(lines, start_idx):
    collected = []
    depth_paren = depth_brack = depth_angle = 0
    for i in range(start_idx, len(lines)):
        ln = lines[i]\
            .replace('\t', ' ')
        collected.append(strip_inline_comments(ln).rstrip())
        for ch in ln:
            if ch == '(':
                depth_paren += 1
            elif ch == ')':
                depth_paren = max(depth_paren-1, 0)
            elif ch == '[':
                depth_brack += 1
            elif ch == ']':
                depth_brack = max(depth_brack-1, 0)
            elif ch == '<':
                depth_angle += 1
            elif ch == '>':
                depth_angle = max(depth_angle-1, 0)
        if depth_paren == depth_brack == 0:
            # terminate on { or ; for item declaration
            if '{' in ln or ';' in ln:
                break
    sig = ' '.join(x.strip() for x in collected if x.strip())
    sig = re.sub(r'\s+', ' ', sig)
    sig = sig.split('{')[0].strip()
    return sig.rstrip()
def crate_for(path):
    parts = path.parts
    idx = parts.index('crates')
    return parts[idx+1]
# ---------- public items ----------
public_items = []
pub_item_re = re.compile(r'^\s*pub\s+(?:(?:const|async|unsafe)\s+)*(fn|struct|
enum|trait)\b')
for path in rs_files:
    text = read_text(path)
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if pub_item_re.match(line) and 'pub(crate)' not in line and 'pub(super)'
not in line and 'pub(in ' not in line:
            kind = pub_item_re.match(line).group(1)
            sig = collect_signature(lines, i)
            public_items.append({
                'crate': crate_for(path),
                'file': rel(path),
                'line': i+1,
                'kind': kind,
                'signature': sig,
            })
# ---------- tests from source ----------
source_tests = []
test_attr_re = re.compile(r'^\s*#\[(test|tokio::test(?:\([^\]]*\))?)\]')
ignore_re = re.compile(r'^\s*#\[ignore(?:\s*=\s*"[^"]*")?\]')
cfg_re = re.compile(r'^\s*#\[cfg\((.+)\)\]')
fn_re = re.compile(r'^\s*fn\s+([A-Za-z_][A-Za-z0-9_]*)\b')
pubfn_re = re.compile(r'^\s*pub\s+(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\b')
for path in rs_files:
    text = read_text(path)
    lines = text.splitlines()
    pending = []
    mod_cfg_stack = []
    brace_depth = 0
    mod_entries = []
    for i, line in enumerate(lines, start=1):
        stripped = line.strip()
        if stripped.startswith('#['):
            pending.append((i, stripped))
        mmod = re.match(r'^\s*(?:pub\s+)?mod\s+tests\b', line)
        if mmod:
            conds = [a for _, a in pending if a.startswith('#[cfg(')]
            mod_entries.append((brace_depth, conds))
            pending = []
        if test_attr_re.match(line):
            pass
        mfn = fn_re.match(line) or pubfn_re.match(line)
        if mfn:
            attrs = pending[:]
            pending = []
            is_test = any(test_attr_re.match(a) for _, a in attrs)
            if is_test:
                conditions = [a for _, a in attrs if cfg_re.match(a)]
                for depth, conds in mod_entries:
                    if brace_depth >= depth:
                        conditions.extend(conds)
                ignored = any(ignore_re.match(a) for _, a in attrs)
                category = 'integration' if '/tests/' in rel(path) else 'unit'
                source_tests.append({
                    'crate': crate_for(path),
                    'file': rel(path),
                    'line': i,
                    'name': mfn.group(1),
                    'category': category,
                    'ignored': ignored,
                    'conditional': sorted(set(conditions)),
                    'attrs': [a for _, a in attrs],
                })
        # update brace depth and mod stack
        brace_depth += line.count('{') - line.count('}')
        mod_entries = [(d, c) for d, c in mod_entries if brace_depth >= d]
        if stripped and not stripped.startswith('#[') and not
stripped.startswith('///') and not stripped.startswith('//'):
            pending = []
# ---------- doctests from cargo output ----------
doctests = []
doctest_file = Path('/tmp/nanobot_doctest_list.txt')
if doctest_file.exists():
    current_crate = None
    for raw in doctest_file.read_text(encoding='utf-8',
errors='replace').splitlines():
        line = raw.strip()
        m = re.match(r'Doc-tests\s+([A-Za-z0-9_\-]+)', line)
        if m:
            current_crate = m.group(1).replace('_', '-')
            if current_crate == 'nanobot-rust':
                current_crate = None
            continue
        if not current_crate:
            continue
        if line.endswith(': test'):
            name = line[:-6]
            file_match = re.match(r'(.+) - .+ \(line (\d+)\)$', name)
            file_ref = None
            ln = None
            if file_match:
                file_ref = file_match.group(1)
                ln = int(file_match.group(2))
            doctests.append({
                'crate': current_crate,
                'name': name,
                'file': file_ref,
                'line': ln,
                'category': 'doc',
                'ignored': False,
                'conditional': [],
            })
# ---------- errors ----------
error_types = []
# public or private enums/structs/type aliases with Error in name
err_def_re = re.compile(r'^\s*(pub\s+)?(enum|struct|type)\s+([A-Za-z_][A-Za-z0-
9_]*Error[A-Za-z0-9_]*)\b')
for path in rs_files:
    text = read_text(path)
    lines = text.splitlines()
    for i, line in enumerate(lines):
        m = err_def_re.match(line)
        if not m:
            continue
        kind = m.group(2)
        name = m.group(3)
        sig = collect_signature(lines, i)
        entry = {
            'crate': crate_for(path), 'file': rel(path), 'line': i+1,
            'kind': kind, 'name': name, 'signature': sig, 'variants': []
        }
        if kind == 'enum':
            depth = 0
            started = False
            for j in range(i, len(lines)):
                ln = lines[j]
                if '{' in ln:
                    depth += ln.count('{')
                    started = True
                    if j == i:
                        continue
                if started and depth == 1:
                    vm = re.match(r'^\s*([A-Z][A-Za-z0-9_]*)\b', ln)
                    if vm and not ln.strip().startswith('#'):
                        entry['variants'].append({'name': vm.group(1), 'line':
j+1, 'text': ln.strip().rstrip(',')})
                if '}' in ln and started:
                    depth -= ln.count('}')
                    if depth <= 0:
                        break
        error_types.append(entry)
# swallowed errors and conversions
swallowed = []
patterns = [
    ('result_ok_discard', re.compile(r'\.ok\(\)')),
    ('ignored_result_binding', re.compile(r'let\s+_\s*=.*;')),
    ('unwrap_or_default', re.compile(r'\.unwrap_or_default\(\)')),
    ('unwrap_or_else', re.compile(r'\.unwrap_or_else\(')),
    ('if_let_err', re.compile(r'if\s+let\s+Err\b')),
]
for path in rs_files:
    lines = read_text(path).splitlines()
    for i, line in enumerate(lines, start=1):
        for kind, pat in patterns:
            if pat.search(line):
                swallowed.append({'crate': crate_for(path), 'file': rel(path),
'line': i, 'kind': kind, 'text': line.strip()})
                break
# ---------- config fields ----------
config_fields = []
schema_path = root/'crates/nanobot-config/src/schema.rs'
schema_text = read_text(schema_path)
schema_lines = schema_text.splitlines()
struct_re = re.compile(r'^\s*pub\s+struct\s+([A-Za-z_][A-Za-z0-9_]*)\b')
field_re = re.compile(r'^\s*pub\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.+?)(,)?\s*$')
structs = []
current = None
brace = 0
for i, line in enumerate(schema_lines, start=1):
    m = struct_re.match(line)
    if m:
        current = m.group(1)
        brace = line.count('{') - line.count('}')
        structs.append(current)
        continue
    if current:
        brace += line.count('{') - line.count('}')
        fm = field_re.match(line)
        if fm and brace >= 1:
            field = fm.group(1)
            ty = fm.group(2).strip().rstrip(',')
            config_fields.append({'struct': current, 'field': field, 'type': ty,
'file': rel(schema_path), 'line': i})
        if brace <= 0:
            current = None
# references of config fields across repo
all_rs_text = {rel(p): read_text(p) for p in rs_files}
for cf in config_fields:
    refs = []
    token_patterns = [re.compile(r'\.' + re.escape(cf['field']) + r'\b'),
re.compile(r'\b' + re.escape(cf['field']) + r'\s*:')]
    for file, text in all_rs_text.items():
        for ln_no, ln in enumerate(text.splitlines(), start=1):
            if file == cf['file'] and ln_no == cf['line']:
                continue
            if any(p.search(ln) for p in token_patterns):
                refs.append(f'{file}:{ln_no}')
    cf['refs'] = refs
    cf['used_non_schema'] = any(not r.startswith(cf['file'] + ':') for r in
refs)
# ---------- comments tags ----------
tag_hits = []
for path in list(root.glob('crates/**/*.rs')) + list(root.glob('src/**/*.rs')) +
list(root.glob('tests/**/*.rs')):
    lines = read_text(path).splitlines()
    for i, line in enumerate(lines, start=1):
        if re.search(r'\b(TODO|FIXME|HACK|XXX)\b', line):
            tag_hits.append({'file': rel(path), 'line': i, 'text':
line.rstrip()})
# ---------- unsafe/unwrap/expect ----------
risk_calls = []
for path in rs_files:
    lines = read_text(path).splitlines()
    for i, line in enumerate(lines, start=1):
        if re.search(r'\bunsafe\b', line):
            risk_calls.append({'kind': 'unsafe', 'file': rel(path), 'line': i,
'text': line.strip()})
        if '.unwrap()' in line or re.search(r'\bunwrap\s*!', line):
            risk_calls.append({'kind': 'unwrap', 'file': rel(path), 'line': i,
'text': line.strip()})
        if '.expect(' in line or re.search(r'\bexpect\s*!', line):
            risk_calls.append({'kind': 'expect', 'file': rel(path), 'line': i,
'text': line.strip()})
# ---------- clone() on likely large types ----------
clone_sites = []
# collect simple type hints per file
for path in rs_files:
    text = read_text(path)
    lines = text.splitlines()
    var_types = {}
    field_types = {}
    fn_param_types = {}
    for i, line in enumerate(lines, start=1):
        m = re.match(r'^\s*(?:pub\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(String|
Vec<[^>]+>|HashMap<[^>]+>|BTreeMap<[^>]+>|Option<String>|Option<Vec<[^>]+>>)',
line)
        if m:
            field_types[m.group(1)] = m.group(2)
        for m in re.finditer(r'\b([a-z_][A-Za-z0-9_]*)\s*:\s*(String|Vec<[^>]+>|
HashMap<[^>]+>|BTreeMap<[^>]+>|Option<String>|Option<Vec<[^>]+>>)', line):
            var_types.setdefault(m.group(1), m.group(2))
        m = re.match(r'^\s*let\s+([a-z_][A-Za-z0-9_]*)\s*:\s*(String|Vec<[^>]+>|
HashMap<[^>]+>|BTreeMap<[^>]+>|Option<String>|Option<Vec<[^>]+>>)', line)
        if m:
            var_types[m.group(1)] = m.group(2)
    for i, line in enumerate(lines, start=1):
        for m in re.finditer(r'\b([A-Za-z_][A-Za-z0-9_\.]+)\.clone\(\)', line):
            expr = m.group(1)
            inferred = None
            base = expr.split('.')[-1]
            if expr.startswith('self.'):
                base = expr.split('.')[-1]
                inferred = field_types.get(base)
            else:
                inferred = var_types.get(base)
            if inferred and any(inferred.startswith(x) for x in ['String',
'Vec<', 'HashMap<', 'BTreeMap<', 'Option<String>', 'Option<Vec<']):
                clone_sites.append({'crate': crate_for(path), 'file': rel(path),
'line': i, 'expr': expr, 'type': inferred, 'text': line.strip()})
# ---------- loc ----------
loc = []
for path in rs_files:
    count = sum(1 for _ in open(path, encoding='utf-8'))
    loc.append({'crate': crate_for(path), 'file': rel(path), 'loc': count})
by_crate_loc = defaultdict(int)
for x in loc:
    by_crate_loc[x['crate']] += x['loc']
# ---------- test counts by crate ----------
all_tests_by_crate = defaultdict(lambda: {'unit': [], 'integration': [], 'doc':
[]})
for t in source_tests:
    all_tests_by_crate[t['crate']][t['category']].append(t)
for t in doctests:
    all_tests_by_crate[t['crate']]['doc'].append(t)
# ---------- write report ----------
out = []
out.append('# White Hat Factual Inventory')
out.append('')
out.append('Generated from workspace sources and Cargo test/doctest listing.')
out.append('')
out.append('## Workspace')
out.append('')
out.append('Crates: ' + ', '.join(crate_names))
out.append('')
out.append('## 1. Public API Inventory')
out.append('')
for crate in crate_names:
    items = [x for x in public_items if x['crate'] == crate]
    out.append(f'### {crate}')
    out.append('')
    out.append(f'Count: {len(items)}')
    out.append('')
    for x in items:
        out.append(f'- `{x["signature"]}` — `{x["file"]}:{x["line"]}`')
    out.append('')
out.append('## 2. Test Inventory')
out.append('')
for crate in crate_names:
    bucket = all_tests_by_crate[crate]
    out.append(f'### {crate}')
    out.append('')
    out.append(f'- Unit: {len(bucket["unit"])}')
    out.append(f'- Integration: {len(bucket["integration"])}')
    out.append(f'- Doc: {len(bucket["doc"])}')
    out.append('')
    for cat in ['unit', 'integration', 'doc']:
        out.append(f'#### {cat}')
        out.append('')
        if not bucket[cat]:
            out.append('- None')
        for t in sorted(bucket[cat], key=lambda x: ((x.get('file') or ''),
x.get('line') or 0, x['name'])):
            extras = []
            if t.get('ignored'):
                extras.append('#[ignore]')
            if t.get('conditional'):
                extras.append('conditional=' + '; '.join(t['conditional']))
            extra = f' ({", ".join(extras)})' if extras else ''
            if cat == 'doc':
                locref = f"{t.get('file')}:{t.get('line')}" if t.get('file')
else 'cargo doctest listing'
                out.append(f'- `{t["name"]}` — `{locref}`{extra}')
            else:
                out.append(f'- `{t["name"]}` — `{t["file"]}:{t["line"]}
`{extra}')
        out.append('')
out.append('## 3. Error Types And Variants')
out.append('')
out.append(f'Error-type definitions found: {len(error_types)}')
out.append('')
for e in error_types:
    out.append(f'- `{e["signature"]}` — `{e["file"]}:{e["line"]}`')
    if e['variants']:
        for v in e['variants']:
            out.append(f'  - `{v["name"]}` — `{e["file"]}:{v["line"]}` —
`{v["text"]}`')
out.append('')
out.append('### Error-discard / conversion sites')
out.append('')
for s in swallowed:
    out.append(f'- `{s["kind"]}` — `{s["file"]}:{s["line"]}` — `{s["text"]}`')
out.append('')
out.append('### Taxonomy completeness')
out.append('')
out.append('- No explicit taxonomy-completeness marker or sealed hierarchy
declaration found in the scanned source.')
out.append('- Factual completeness basis in this report: all source definitions
whose type name matches `*Error*`, plus all enum variants in those
definitions.')
out.append('')
out.append('## 4. YAML Schema Fields')
out.append('')
for struct_name in sorted({x['struct'] for x in config_fields}):
    fields = [x for x in config_fields if x['struct'] == struct_name]
    out.append(f'### {struct_name}')
    out.append('')
    for f in fields:
        status = 'used' if f['used_non_schema'] else 'no non-schema reference
found'
        refs = ', '.join(f['refs'][:20])
        if len(f['refs']) > 20:
            refs += f', ... (+{len(f["refs"])-20} more)'
        out.append(f'- `{f["field"]}: {f["type"]}` — `{f["file"]}:{f["line"]}` —
{status}')
        out.append(f'  - refs: {refs if refs else "none"}')
    out.append('')
out.append('## 5. TODO / FIXME / HACK / XXX')
out.append('')
for h in tag_hits:
    out.append(f'- `{h["file"]}:{h["line"]}` — `{h["text"]}`')
out.append('')
out.append('## 6. unsafe / unwrap / expect')
out.append('')
for r in risk_calls:
    out.append(f'- `{r["kind"]}` — `{r["file"]}:{r["line"]}` — `{r["text"]}`')
out.append('')
out.append('## 7. clone() On Likely Large Types')
out.append('')
out.append('Heuristic basis: direct `.clone()` receiver with locally inferable
type `String`, `Vec<_>`, `HashMap<_,_>`, `BTreeMap<_,_>`, `Option<String>`, or
`Option<Vec<_>>`.')
out.append('')
for c in clone_sites:
    out.append(f'- `{c["expr"]}.clone()` type `{c["type"]}` — `{c["file"]}:
{c["line"]}` — `{c["text"]}`')
out.append('')
out.append('## 8. Module Sizes')
out.append('')
out.append('### Per file LOC')
out.append('')
for entry in sorted(loc, key=lambda x: (-x['loc'], x['file'])):
    out.append(f'- `{entry["file"]}` — {entry["loc"]} LOC')
out.append('')
out.append('### Per crate LOC')
out.append('')
for crate, total in sorted(by_crate_loc.items(), key=lambda kv: (-kv[1],
kv[0])):
    out.append(f'- `{crate}` — {total} LOC')
out.append('')
out.append('### Largest modules')
out.append('')
for entry in sorted(loc, key=lambda x: (-x['loc'], x['file']))[:25]:
    out.append(f'- `{entry["file"]}` — {entry["loc"]} LOC')
out.append('')
report_path = root / 'WHITEHAT_INVENTORY.md'
report_path.write_text('\n'.join(out) + '\n', encoding='utf-8')
print(report_path)
print(f'public_items={len(public_items)} source_tests={len(source_tests)}
doctests={len(doctests)} error_types={len(error_types)}
config_fields={len(config_fields)} tags={len(tag_hits)}
risk_calls={len(risk_calls)} clone_sites={len(clone_sites)}')
PY

• Explored
  └ Read nanobot_doctest_list.txt, nanobot_test_list.txt

• Ran python3 - <<'PY'
  │ import os, re, json
  │ from pathlib import Path
  │ … +512 lines
  └ /opt/nanobot-rust/nanobot-rust/WHITEHAT_INVENTORY.md
    public_items=803 source_tests=1128 doctests=5 error_types=4
    config_fields=132 tags=4 risk_calls=2397 clone_sites=118

────────────────────────────────────────────────────────────────────────────────

• The report is written. I’m checking the generated inventory against a few
  representative files and pulling summary counts so the handoff can point you
  straight to the artifact and any extraction limits.

• Explored
  └ Read WHITEHAT_INVENTORY.md
    Search ^##  in WHITEHAT_INVENTORY.md
    Search ^### nanobot- in WHITEHAT_INVENTORY.md
    Search ^- `#[^`]*ignore|\(#\[ignore\]\) in WHITEHAT_INVENTORY.md

────────────────────────────────────────────────────────────────────────────────

• The inventory is in WHITEHAT_INVENTORY.md:1.

  It includes all requested sections with file:line references:

  - public API inventory across all 16 crates: 803 pub fn / pub struct / pub
    enum / pub trait entries
  - test inventory by crate with unit/integration/doc categorization from source
    plus Cargo doctest listing: 1128 source tests, 5 doctests
  - error type/variant inventory plus explicit error-discard/conversion sites
  - YAML schema field inventory with observed reference sites and used vs no
    non-schema reference found
  - all TODO / FIXME / HACK / XXX hits
  - all unsafe, unwrap(), expect() hits
  - clone() inventory for directly inferable large types
  - per-file LOC, per-crate LOC, and largest modules

  One extraction note is embedded in the report: the clone() section is
  heuristic and only includes .clone() calls where the receiver type is directly
  inferable from local syntax as String, Vec<_>, HashMap<_,_>, BTreeMap<_,_>,
  Option<String>, or Option<Vec<_>>.


› codex --full-auto


  gpt-5.4 default · /opt/nanobot-rust/nanobot-rust

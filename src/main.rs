use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use git2::Repository;

fn load_ignore() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!("{}/.gitc-ignore", home);
    match fs::read_to_string(&path) {
        Ok(text) => text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => {
            eprintln!("note: no ~/.gitc-ignore found, using empty ignore list");
            vec![]
        }
    }
}

struct Repo {
    path: String,
    count: u32,
    first: NaiveDate,
    last: NaiveDate,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let ignore = load_ignore();

    eprintln!(
        "scanning {} for .git dirs ({} ignore patterns)...",
        home,
        ignore.len()
    );
    let dirs = find_git_dirs(&home, &ignore)?;
    eprintln!("found {} repos", dirs.len());

    let mut all_repos: Vec<Repo> = Vec::new();
    let mut totals_by_day: BTreeMap<NaiveDate, u32> = BTreeMap::new();

    for (i, path) in dirs.iter().enumerate() {
        eprint!("\r{:04}/{:04} {}\x1b[K", i + 1, dirs.len(), path);
        io::stderr().flush().ok();

        match commits_for_repo(path) {
            Ok(days) if !days.is_empty() => {
                let count: u32 = days.values().sum();
                let first = *days.keys().next().unwrap();
                let last = *days.keys().next_back().unwrap();
                for (d, n) in &days {
                    *totals_by_day.entry(*d).or_insert(0) += n;
                }
                all_repos.push(Repo {
                    path: path.clone(),
                    count,
                    first,
                    last,
                });
            }
            _ => {}
        }
    }
    eprintln!("\nprocessed {} repos with commits by you", all_repos.len());

    all_repos.sort_by(|a, b| b.count.cmp(&a.count));

    // monthly aggregation
    let monthly = aggregate_monthly(&totals_by_day);

    let html = render_html(&monthly, &all_repos);
    let out_path = "/tmp/gitc.html";
    fs::write(out_path, &html)?;
    eprintln!("wrote {}", out_path);

    // auto open in browser
    Command::new("open").arg(out_path).status().ok();

    Ok(())
}

fn find_git_dirs(home: &str, ignore: &[String]) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut cmd = Command::new("fd");
    cmd.args(&["-HI", "-t", "d", r"^\.git$", home]);
    for ig in ignore {
        cmd.arg("-E").arg(ig);
    }
    let out = cmd.output()?;
    let text = String::from_utf8_lossy(&out.stdout);

    let mut dirs: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_end_matches('/').trim_end_matches("/.git");
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

fn commits_for_repo(path: &str) -> Result<BTreeMap<NaiveDate, u32>, Box<dyn std::error::Error>> {
    let repo = Repository::open(path)?;
    let me = match repo_author(&repo) {
        Some(n) => n,
        None => return Ok(BTreeMap::new()),
    };
    let mut walk = repo.revwalk()?;
    walk.push_glob("refs/heads/*")?;
    walk.push_glob("refs/remotes/*")?;

    let mut by_day: BTreeMap<NaiveDate, u32> = BTreeMap::new();
    let mut seen: std::collections::HashSet<git2::Oid> = std::collections::HashSet::new();

    for oid_result in walk {
        let oid = match oid_result {
            Ok(o) => o,
            Err(_) => continue,
        };
        if !seen.insert(oid) {
            continue;
        }
        let commit = match repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let author = commit.author();
        let name = author.name().unwrap_or("");
        if !name.eq_ignore_ascii_case(&me) {
            continue;
        }
        let secs = commit.time().seconds();
        let date = match Utc.timestamp_opt(secs, 0).single() {
            Some(dt) => dt.naive_utc().date(),
            None => continue,
        };
        *by_day.entry(date).or_insert(0) += 1;
    }
    Ok(by_day)
}

fn repo_author(repo: &Repository) -> Option<String> {
    let cfg = repo.config().ok()?;
    cfg.get_string("user.name").ok().or_else(|| {
        git2::Config::open_default()
            .ok()?
            .get_string("user.name")
            .ok()
    })
}

fn aggregate_monthly(by_day: &BTreeMap<NaiveDate, u32>) -> Vec<(NaiveDate, u32)> {
    let mut by_month: BTreeMap<(i32, u32), u32> = BTreeMap::new();
    for (d, n) in by_day {
        *by_month.entry((d.year(), d.month())).or_insert(0) += n;
    }
    if by_month.is_empty() {
        return vec![];
    }

    let (first_y, _) = *by_month.keys().next().unwrap();

    // extend through current month
    let today = chrono::Local::now().date_naive();
    let last_y = today.year();
    let last_m = today.month();

    let mut out = Vec::new();
    let mut y = first_y;
    let mut m = 1u32;
    loop {
        let count = by_month.get(&(y, m)).copied().unwrap_or(0);
        out.push((NaiveDate::from_ymd_opt(y, m, 1).unwrap(), count));
        if y == last_y && m == last_m {
            break;
        }
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    out
}

fn render_html(monthly: &[(NaiveDate, u32)], repos: &[Repo]) -> String {
    // unix timestamps in seconds for uplot
    let xs: Vec<i64> = monthly
        .iter()
        .map(|(d, _)| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
        .collect();
    let ys: Vec<u32> = monthly.iter().map(|(_, c)| *c).collect();

    let xs_json = serde_json_array_i64(&xs);
    let ys_json = serde_json_array_u32(&ys);

    let mut rows = String::new();
    for r in repos.iter() {
        rows.push_str(&format!(
            "<tr><td class=\"c\" data-sort=\"{}\">{}</td><td class=\"d\" data-sort=\"{}\">{}</td><td class=\"d\" data-sort=\"{}\">{}</td><td>{}</td></tr>\n",
            r.count, r.count,
            r.first, r.first,
            r.last, r.last,
            html_escape(&r.path)
        ));
    }

    let total_commits: u32 = repos.iter().map(|r| r.count).sum();
    let total_repos = repos.len();

    let template = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>gitc</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/uplot@1.6.31/dist/uPlot.min.css">
<script src="https://cdn.jsdelivr.net/npm/uplot@1.6.31/dist/uPlot.iife.min.js"></script>
<style>
  :root {
    --bg: #0f0f0f;
    --fg: #e6e6e6;
    --dim: #888;
    --green: #4ade80;
    --border: #2a2a2a;
  }
  * { box-sizing: border-box; }
  html, body { margin: 0; padding: 0; background: var(--bg); color: var(--fg); font-family: ui-monospace, SF Mono, Menlo, monospace; }
  body { padding: 24px; }
  h1 { font-size: 14px; font-weight: 500; color: var(--dim); margin: 0 0 4px 0; letter-spacing: 0.5px; text-transform: uppercase; }
  .stats { font-size: 12px; color: var(--dim); margin-bottom: 16px; }
  .stats b { color: var(--fg); font-weight: 500; }
  #chart { background: #141414; border: 1px solid var(--border); border-radius: 6px; padding: 12px; margin-bottom: 24px; }
  .table-wrap { background: #141414; border: 1px solid var(--border); border-radius: 6px; max-height: 320px; overflow-y: auto; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 8px 14px; border-bottom: 1px solid var(--border); }
  th { position: sticky; top: 0; background: #1a1a1a; color: var(--dim); font-weight: 500; text-transform: uppercase; font-size: 11px; letter-spacing: 0.5px; }
  th.sortable { cursor: pointer; user-select: none; }
  th.sortable:hover { color: var(--fg); }
  th.sorted-asc::after { content: " \2191"; color: var(--green); }
  th.sorted-desc::after { content: " \2193"; color: var(--green); }
  tr:last-child td { border-bottom: none; }
  td.c { color: var(--green); width: 100px; font-variant-numeric: tabular-nums; }
  td.d { color: var(--dim); width: 130px; font-variant-numeric: tabular-nums; }
  .u-axis { color: var(--dim); }
  .uplot { font-family: inherit; }
  .u-legend { color: var(--fg); }
  .u-legend th, .u-legend td { border: none; padding: 2px 8px; }
</style>
</head>
<body>
  <h1>Commits per month</h1>
  <div class="stats"><b>__TOTAL_COMMITS__</b> commits across <b>__TOTAL_REPOS__</b> repos</div>
  <div id="chart"></div>

  <h1 style="margin-top:8px">All repos</h1>
  <div class="stats">click a column header to sort</div>
  <div class="table-wrap">
    <table id="repos">
      <thead><tr>
        <th class="c sortable sorted-desc" data-col="0" data-type="num">commits</th>
        <th class="d sortable" data-col="1" data-type="date">first commit</th>
        <th class="d sortable" data-col="2" data-type="date">last commit</th>
        <th class="sortable" data-col="3" data-type="text">repo</th>
      </tr></thead>
      <tbody>
__ROWS__      </tbody>
    </table>
  </div>

<script>
const xs = __XS__;
const ys = __YS__;

const opts = {
  width: Math.min(window.innerWidth - 48, 1600),
  height: 360,
  scales: { x: { time: true } },
  axes: [
    { stroke: "#888", grid: { stroke: "#222", width: 1 }, ticks: { stroke: "#444" } },
    { stroke: "#888", grid: { stroke: "#222", width: 1 }, ticks: { stroke: "#444" } },
  ],
  series: [
    {},
    {
      label: "commits",
      stroke: "#4ade80",
      fill: "rgba(74, 222, 128, 0.25)",
      width: 1.5,
      points: { show: false },
      paths: uPlot.paths.bars({ size: [0.85, 60] }),
    },
  ],
};

const data = [xs, ys];
const u = new uPlot(opts, data, document.getElementById("chart"));

window.addEventListener("resize", () => {
  u.setSize({ width: Math.min(window.innerWidth - 48, 1600), height: 360 });
});

// sortable table
(function() {
  const table = document.getElementById("repos");
  const tbody = table.querySelector("tbody");
  const headers = table.querySelectorAll("th.sortable");

  function sortBy(col, type, dir) {
    const rows = Array.from(tbody.querySelectorAll("tr"));
    rows.sort((a, b) => {
      const av = a.children[col].dataset.sort ?? a.children[col].textContent;
      const bv = b.children[col].dataset.sort ?? b.children[col].textContent;
      let cmp;
      if (type === "num") cmp = Number(av) - Number(bv);
      else cmp = av.localeCompare(bv);
      return dir === "asc" ? cmp : -cmp;
    });
    rows.forEach(r => tbody.appendChild(r));
  }

  headers.forEach(th => {
    th.addEventListener("click", () => {
      const col = +th.dataset.col;
      const type = th.dataset.type;
      const isAsc = th.classList.contains("sorted-asc");
      const newDir = isAsc ? "desc" : "asc";
      headers.forEach(h => h.classList.remove("sorted-asc", "sorted-desc"));
      th.classList.add(newDir === "asc" ? "sorted-asc" : "sorted-desc");
      sortBy(col, type, newDir);
    });
  });
})();
</script>
</body>
</html>
"##;

    template
        .replace("__TOTAL_COMMITS__", &total_commits.to_string())
        .replace("__TOTAL_REPOS__", &total_repos.to_string())
        .replace("__ROWS__", &rows)
        .replace("__XS__", &xs_json)
        .replace("__YS__", &ys_json)
}

fn serde_json_array_i64(v: &[i64]) -> String {
    let mut s = String::with_capacity(v.len() * 12);
    s.push('[');
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&n.to_string());
    }
    s.push(']');
    s
}

fn serde_json_array_u32(v: &[u32]) -> String {
    let mut s = String::with_capacity(v.len() * 4);
    s.push('[');
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&n.to_string());
    }
    s.push(']');
    s
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[allow(dead_code)]
fn _ensure_path_used(_p: &Path) {} // silence unused import warning 

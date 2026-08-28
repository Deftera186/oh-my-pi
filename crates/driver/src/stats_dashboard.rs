//! Immutable embedded stats dashboard assets.

/// Versioned immutable dashboard asset.
pub struct Asset {
	/// HTTP content type.
	pub content_type: &'static str,
	/// Cache validator tied to the embedded payload.
	pub etag:         &'static str,
	/// Asset bytes.
	pub bytes:        &'static [u8],
}

const DASHBOARD: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="light dark"><title>OMP statistics</title>
<style>
:root{font:14px/1.45 system-ui,sans-serif;color-scheme:light dark;--bg:#f5f6f8;--panel:#fff;--ink:#17191d;--muted:#69707c;--line:#dfe2e8;--accent:#1769e0}html[data-theme=dark]{--bg:#111318;--panel:#191c22;--ink:#eef0f4;--muted:#a5abb6;--line:#303640;--accent:#75aaff}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink)}header{position:sticky;top:0;z-index:2;display:flex;gap:12px;align-items:center;padding:14px 20px;border-bottom:1px solid var(--line);background:var(--panel)}header strong{font-size:18px}button,select{border:1px solid var(--line);border-radius:7px;background:var(--panel);color:var(--ink);padding:7px 10px}button{cursor:pointer}.spacer{flex:1}nav{display:flex;gap:4px;overflow:auto;padding:10px 20px;border-bottom:1px solid var(--line)}nav button.active{background:var(--accent);color:white;border-color:var(--accent)}main{max-width:1200px;margin:auto;padding:20px}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:12px}.card{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:16px}.metric{font-size:26px;font-weight:650}.muted{color:var(--muted)}table{width:100%;border-collapse:collapse;background:var(--panel)}th,td{text-align:left;padding:9px;border-bottom:1px solid var(--line)}pre{white-space:pre-wrap;overflow-wrap:anywhere}.state{padding:36px;text-align:center;background:var(--panel);border:1px solid var(--line);border-radius:10px}.error{color:#d33}.model-share{display:grid;gap:8px;margin-bottom:16px}.model-share-row{display:grid;grid-template-columns:minmax(120px,220px) 1fr auto;gap:10px;align-items:center}.model-share-track{height:9px;border-radius:99px;background:var(--line);overflow:hidden}.model-share-fill{display:block;height:100%;border-radius:inherit}.model-swatch{display:inline-block;width:9px;height:9px;border-radius:50%;margin-right:7px}@media(max-width:600px){header{padding:10px}nav,main{padding:10px}.model-share-row{grid-template-columns:minmax(80px,140px) 1fr auto}}
</style></head><body><header><strong>OMP statistics</strong><span id="status" class="muted"></span><span class="spacer"></span><select id="range" aria-label="Time range"><option value="24h">24 hours</option><option value="7d">7 days</option><option value="30d" selected>30 days</option><option value="90d">90 days</option><option value="all">All time</option></select><button id="sync">Sync</button><button id="theme" aria-label="Toggle theme">Theme</button></header><nav id="nav"></nav><main id="main"><div class="state">Loading statistics...</div></main>
<script>
const routes={overview:'overview',requests:'recent',errors:'errors',models:'models',providers:'providers',tools:'tools',costs:'costs',behavior:'behavior',projects:'folders',gain:'gain'};let controller;
const $=id=>document.getElementById(id), esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const MODEL_COLORS=['#ed4abf','#9b4dff','#5ad8e6','#62d394','#c77dff','#ff8fd1','#f5c14b','#ff6b7d'];
const modelName=row=>String(row?.key?.model??row?.model??'unknown');
const modelKey=row=>`${modelName(row)}::${String(row?.key?.provider??row?.provider??'')}`;
function buildModelColorLookup(records){const ranked=[...records].sort((a,b)=>(Number(b.requests)||0)-(Number(a.requests)||0)||modelKey(a).localeCompare(modelKey(b)));return new Map(ranked.map((record,index)=>[modelKey(record),MODEL_COLORS[index%MODEL_COLORS.length]]))}
function modelShareChart(rows,colorLookup){const total=rows.reduce((sum,row)=>sum+(Number(row.requests)||0),0);if(total<=0)return '';return `<section class="card"><h2>Request share</h2><div class="model-share">${rows.map(row=>{const name=modelName(row),requests=Number(row.requests)||0,color=colorLookup.get(modelKey(row)),share=requests/total*100;return `<div class="model-share-row"><span>${esc(name)}</span><span class="model-share-track"><span class="model-share-fill" style="width:${share.toFixed(2)}%;background:${color}"></span></span><span class="muted">${share.toFixed(1)}%</span></div>`}).join('')}</div></section>`}
function modelsTable(rows,colorLookup){if(!rows.length)return '<div class="state">No records in this range.</div>';return `<table><thead><tr><th>Model</th><th>Requests</th><th>Errors</th><th>Usage</th><th>Cost</th></tr></thead><tbody>${rows.map(row=>{const name=modelName(row),color=colorLookup.get(modelKey(row));return `<tr><td><span class="model-swatch" style="background:${color}"></span>${esc(name)}</td><td>${esc(row.requests)}</td><td>${esc(row.errors)}</td><td><pre>${esc(JSON.stringify(row.usage??{},null,2))}</pre></td><td>${esc(row.cost_nanos_usd)}</td></tr>`}).join('')}</tbody></table>`}
function models(data){const rows=Array.isArray(data)?data:data.rows||[];if(!rows.length)return '<div class="state">No records in this range.</div>';const colorLookup=buildModelColorLookup(rows);return `${modelShareChart(rows,colorLookup)}${modelsTable(rows,colorLookup)}`}
function route(){const key=location.hash.slice(1);return routes[key]?key:'overview'}
function headers(){const token=sessionStorage.getItem('omp-stats-token');return token?{accept:'application/json',authorization:`Bearer ${token}`}:{accept:'application/json'}}
function nav(){const active=route();$('nav').innerHTML=Object.keys(routes).map(k=>`<button data-route="${k}" class="${k===active?'active':''}">${k[0].toUpperCase()+k.slice(1)}</button>`).join('');document.querySelectorAll('[data-route]').forEach(b=>b.onclick=()=>location.hash=b.dataset.route)}
function cards(data){const o=data.overall||data;const entries=Object.entries(o).filter(([,v])=>['number','string'].includes(typeof v));if(!entries.length)return '<div class="state">No statistics in this range.</div>';return `<div class="grid">${entries.map(([k,v])=>`<section class="card"><div class="muted">${esc(k.replaceAll('_',' '))}</div><div class="metric">${esc(v)}</div></section>`).join('')}</div>`}
function table(data){const rows=Array.isArray(data)?data:data.rows||data.items||[];if(!rows.length)return '<div class="state">No records in this range.</div>';const cols=[...new Set(rows.flatMap(Object.keys))];return `<table><thead><tr>${cols.map(c=>`<th>${esc(c)}</th>`).join('')}</tr></thead><tbody>${rows.map(r=>`<tr>${cols.map(c=>`<td>${typeof r[c]==='object'?`<pre>${esc(JSON.stringify(r[c],null,2))}</pre>`:esc(r[c])}</td>`).join('')}</tr>`).join('')}</tbody></table>`}
async function load(){controller?.abort();controller=new AbortController();nav();$('main').innerHTML='<div class="state">Loading statistics...</div>';const key=route(),range=$('range').value;try{const r=await fetch(`/api/v1/stats/${routes[key]}?range=${range}`,{signal:controller.signal,headers:headers()});if(r.status===401){const token=prompt('Statistics access token');if(token){sessionStorage.setItem('omp-stats-token',token);return load()}}if(!r.ok)throw new Error(`HTTP ${r.status}`);const envelope=await r.json();$('status').textContent=envelope.meta?.range||range;$('main').innerHTML=key==='overview'?cards(envelope.data):key==='models'?models(envelope.data):table(envelope.data)}catch(e){if(e.name!=='AbortError')$('main').innerHTML=`<div class="state error">Could not load statistics: ${esc(e.message)}<br><button onclick="load()">Retry</button></div>`}}
$('sync').onclick=async()=>{const b=$('sync');b.disabled=true;b.textContent='Syncing...';try{const r=await fetch('/api/v1/stats/sync',{method:'POST',headers:headers()});if(!r.ok)throw new Error(`HTTP ${r.status}`);await load()}catch(e){$('main').innerHTML=`<div class="state error">Sync failed: ${esc(e.message)}</div>`}finally{b.disabled=false;b.textContent='Sync'}};
$('range').onchange=load;$('theme').onclick=()=>{const root=document.documentElement;const next=root.dataset.theme==='dark'?'light':'dark';root.dataset.theme=next;localStorage.setItem('omp-stats-theme',next)};const saved=localStorage.getItem('omp-stats-theme');if(saved)document.documentElement.dataset.theme=saved;addEventListener('hashchange',load);load();
</script></body></html>"##;

/// Looks up an embedded production dashboard asset.
pub fn asset(path: &str) -> Option<Asset> {
	match path {
		"/" | "/index.html" => Some(Asset {
			content_type: "text/html; charset=utf-8",
			etag:         "\"omp-stats-dashboard-v2\"",
			bytes:        DASHBOARD.as_bytes(),
		}),
		_ => None,
	}
}
#[cfg(test)]
mod tests {
	use super::DASHBOARD;

	#[test]
	fn model_color_lookup_is_request_ranked_and_shared_by_both_views() {
		let mut records = [
			("Luna::provider-luna", 10_505_u64),
			("Sol::provider-sol", 389),
			("Fable::provider-fable", 106),
			("Opus::provider-opus", 191),
			("Shared::provider-z", 5),
			("Shared::provider-a", 5),
		];
		records.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
		let actual = records.map(|record| record.0);
		assert_eq!(
			actual,
			[
				"Luna::provider-luna",
				"Sol::provider-sol",
				"Opus::provider-opus",
				"Fable::provider-fable",
				"Shared::provider-a",
				"Shared::provider-z",
			],
			"request-ranked model keys={actual:?}"
		);

		let ranking = "[...records].sort((a,b)=>(Number(b.requests)||0)-(Number(a.\
		               requests)||0)||modelKey(a).localeCompare(modelKey(b)))";
		assert!(
			DASHBOARD.contains(ranking),
			"dashboard ranking source did not contain {ranking:?}; actual={DASHBOARD}"
		);
		for consumer in ["modelShareChart(rows,colorLookup)", "modelsTable(rows,colorLookup)"] {
			assert!(
				DASHBOARD.contains(consumer),
				"missing color lookup consumer {consumer:?}; actual={DASHBOARD}"
			);
		}
		let lookup_creation = "const colorLookup=buildModelColorLookup(rows)";
		assert!(
			DASHBOARD.contains(lookup_creation),
			"missing deterministic lookup creation {lookup_creation:?}; actual={DASHBOARD}"
		);
		let palette =
			"['#ed4abf','#9b4dff','#5ad8e6','#62d394','#c77dff','#ff8fd1','#f5c14b','#ff6b7d']";
		assert!(
			DASHBOARD.contains(palette),
			"request-ranked palette missing; expected={palette}, actual={DASHBOARD}"
		);
	}
}

//! A small browser dashboard. The page is static and carries no data; it asks
//! for the proxy key, keeps it in `sessionStorage`, and polls
//! `/teamclaude/status` with `x-api-key`, which is what lets a browser request
//! through the auth gate.

pub const HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>TeamClaude</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
:root{color-scheme:light dark;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px}
body{margin:1.5rem;max-width:1100px}h1{font-size:1.1rem;margin:0 0 .5rem}
table{border-collapse:collapse;width:100%;margin-top:1rem}th,td{text-align:left;padding:.3rem .5rem;border-bottom:1px solid #8884}
.bar{display:inline-block;width:80px;height:8px;background:#8883;vertical-align:middle;margin-right:.4rem}.bar i{display:block;height:100%;background:#3a7}
.bar i.hot{background:#c44}.cur{font-weight:700}.dim{opacity:.6}button{font:inherit;cursor:pointer}#err{color:#c44}
#log{white-space:pre-wrap;max-height:14rem;overflow:auto;background:#8881;padding:.5rem;margin-top:1rem}
</style></head><body>
<h1>TeamClaude <span id="meta" class="dim"></span></h1>
<div id="err"></div>
<div id="auth"><label>Proxy key <input id="key" type="password" size="40"></label> <button id="save">Connect</button></div>
<table id="t"><thead><tr><th></th><th>account</th><th>pri</th><th>5h</th><th>7d</th><th>7d fable</th><th>state</th><th></th></tr></thead><tbody></tbody></table>
<div id="log" class="dim"></div>
<script>
(function(){
const $=s=>document.querySelector(s);
let key=sessionStorage.getItem('tcKey')||'';
$('#save').onclick=()=>{key=$('#key').value.trim();sessionStorage.setItem('tcKey',key);tick();};
function bar(b){if(!b||b.utilization==null)return '<span class="dim">?</span>';const u=Math.min(1,Math.max(0,b.utilization));
 const cd=b.resetInSeconds==null?'':' '+fmt(b.resetInSeconds);return '<span class="bar"><i class="'+(u>=.9?'hot':'')+'" style="width:'+(u*100)+'%"></i></span>'+Math.round(u*100)+'%'+cd;}
function fmt(s){if(s<=0)return 'now';const d=Math.floor(s/86400),h=Math.floor(s%86400/3600),m=Math.floor(s%3600/60);return d?d+'d'+h+'h':h?h+'h'+m+'m':m+'m';}
function esc(s){return String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));}
async function api(path,opts){const r=await fetch(path,Object.assign({headers:{'x-api-key':key,'content-type':'application/json'}},opts||{}));if(r.status===401)throw new Error('unauthorized: enter the proxy key');return r.json();}
async function tick(){try{const st=await api('/teamclaude/status');$('#err').textContent='';$('#auth').style.display='none';
 $('#meta').textContent='v'+st.version+' · current: '+(st.current||'-')+' · sessions '+st.sessions.active+'/'+st.sessions.known;
 const tb=$('#t tbody');tb.innerHTML='';
 for(const a of st.accounts){const tr=document.createElement('tr');if(a.current)tr.className='cur';if(a.blocked)tr.classList.add('dim');
  tr.innerHTML='<td>'+(a.current?'►':'')+'</td><td>'+esc(a.name)+'</td><td>'+a.priority+'</td><td>'+bar(a.quota.unified5h)+'</td><td>'+bar(a.quota.unified7d)+'</td><td>'+bar(a.quota.unified7dFable)+'</td><td>'+esc(a.disabled?'disabled':(a.blocked||'ready'))+'</td><td></td>';
  const b=document.createElement('button');b.textContent='use';b.onclick=async()=>{await api('/teamclaude/switch',{method:'POST',body:JSON.stringify({account:a.id})});tick();};tr.lastChild.appendChild(b);tb.appendChild(tr);}
 }catch(e){$('#err').textContent=e.message;$('#auth').style.display='';}}
tick();setInterval(tick,3000);
})();
</script></body></html>"#;

import { confirmDeskAction, renderOperator } from "./operator.js";
const $ = id => document.getElementById(id);
const byId = (n, prefix) => prefix + String(n).padStart(2, "0");
const mixes = ["Main LR", "Aux 1", "Aux 2", "FX 1", "FX 2", "FX 3", "FX 4"];
const busNumbers = [0, 1, 2, 7, 8, 9, 10];
const sendTaps = ["Input","Pre EQ","Post EQ","Pre fader","Post fader","Group"];
const pages = ["Auto-KJ", "Routing", "EQ", "Compressor", "Limiter", "Effects", "Sends"];
const algorithms = "HALL CHAM ROOM RPLT PLAT AMBI GATE RVRS VRM VREV CRS FLNG PIT PIT2 CR/R FL/R D/RV D/CR D/FL MODD DLY 3TAP 4TAP GEQ GEQ2 TEQ TEQ2 PHAS ROTA WAVD LIM ENH ENH2 EXC EXC2 AMP AMP2 DRV DRV2 IMG PAN FILT DES DES2 P1A P1A2 PQ5 PQ52 FAC FAC1M FAC2 LEC LEC2 ULC ULC2 SON EDI SUB DIMC CMB CMB2".split(" ");
const strips = [
    ...Array.from({length: 9}, (_, i) => [byId(i+1,"/ch/"),"Input "+(i+1)]),
    ["/bus/1","Aux 1"], ["/bus/2","Aux 2"],
    ...Array.from({length: 4}, (_, i) => ["/rtn/"+(i+1),"FX "+(i+1)]), ["/lr","Main LR"]
];
const fragment = new URLSearchParams(location.hash.slice(1));
if (fragment.has("token")) {
    sessionStorage.setItem("studio.operator",fragment.get("token"));
    history.replaceState(null,"",location.pathname);
}
let token = sessionStorage.getItem("studio.operator");
let state, page = 0, selected = 0, bank = 0, layoutGeneration = 0, fx = 1;
let bindings = [], statusText = "", lastSyncError = "", noticeTimer;
let meterBindings = [];
let renderedFx;
let transportBindings;
const programMeters=new Map();
let layoutRig="";
document.body.classList.toggle("flat",localStorage.getItem("studio.flat")==="1");
function applyInputs(ids){
    for(let i=0;i<9;i++)strips[i]=[byId(ids[i],"/ch/"),"Input "+ids[i]];
}
function restoreLayout(identity){
    if(!identity)return;
    const rig=identity.address+"|"+identity.model+"|"+identity.name;
    if(rig===layoutRig)return;
    layoutRig=rig;
    let ids;
    try{ids=JSON.parse(localStorage.getItem("studio.inputs."+rig));}catch{/* New rig */}
    if(!Array.isArray(ids)||ids.length!==9||new Set(ids).size!==9||ids.some(v=>!Number.isInteger(v)||v<1||v>12)){
        ids=Array.from({length:9},(_,i)=>i+1);
    }
    applyInputs(ids);build();
}
const busy = new Set();
const confirmedFaders = new Map();
let syncRequestNumber = 0;
function el(tag, text, cls) {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (cls) node.className = cls;
    return node;
}
function button(text, fn, label) {
    const b = el("button",text);
    b.onclick = fn;
    if (label) b.setAttribute("aria-label",label);
    return b;
}
function notify(text, duration=0) {
    clearTimeout(noticeTimer);
    $("notice").textContent=text;
    if(duration)noticeTimer=setTimeout(()=>{$("notice").textContent="";},duration);
}
function observed(key) {
    const mixer = state?.mixer;
    if (!mixer?.connected || !mixer.values[key]) return undefined;
    const confirmed = confirmedFaders.get(key);
    return confirmed?.epoch === mixer.epoch ? confirmed.value : mixer.values[key].value;
}
function stripName(i) { return observed(strips[i][0]+"/config/name") || strips[i][1]; }
function hasAnalyzerAlert(root) {
    const reading=state?.mixer?.analyzer;
    if(!reading || (!reading.competing && reading.bin==null))return false;
    if(reading.source===31)return root==="/lr";
    if(reading.source>=17 && reading.source<=20)return root==="/rtn/"+(reading.source-16);
    if(reading.source===21 || reading.source===22)return root==="/bus/"+(reading.source-20);
    return reading.source<12 && root===byId(reading.source+1,"/ch/");
}
function socketSource(id){
    if(!Number.isInteger(id))return "—";
    if(id<16)return "Input "+(id+1);
    if(id<18)return "Aux input "+(id===16?"L":"R");
    if(id<26)return "FX return "+(1+Math.floor((id-18)/2))+(id%2?" R":" L");
    if(id<32)return "Bus "+(id-25);
    if(id<36)return "FX send "+(id-31);
    if(id<38)return "Main "+(id===36?"L":"R");
    return "USB "+(id-37);
}
function pathFor(root, mix) {
    return mix ? root + "/mix/" + byId(busNumbers[mix],"") + "/level" : root + "/mix/fader";
}
function db(n, precision=1) {
    if (n === 0) return "−∞ dB";
    const value = n >= .5 ? 40*n-30 : n >= .25 ? 80*n-50 : n >= .0625 ? 160*n-70 : 480*n-90;
    return value.toFixed(precision)+" dB";
}
function time(n) {
    if (!Number.isFinite(n) || n < 0) return "—";
    return Math.floor(n/60)+":"+String(Math.floor(n%60)).padStart(2,"0");
}
const nativeRequests = new Map();
let sequence = 0;
window.studioReply = (id,status,body)=>{
    const finish=nativeRequests.get(id);
    if(finish) finish(status,body);
};
async function api(path, data) {
    const readOnly=path==="sync" || path==="meters" || path==="state" || (path==="operator" && !data?.ticket);
    if(!readOnly && (document.hidden || !state)) throw new Error("Wait for a fresh connection before changing the show or mixer.");
    const deadline=path==="operator" || path==="show"?30000:3500;
    const timedOut=data===undefined || path==="sync"?"The show host timed out":"No confirmation received. Refresh the current state before repeating the action.";
    if(window.StudioTablet){
        return new Promise((resolve,reject)=>{
            const id=++sequence;
            const timer=setTimeout(()=>{nativeRequests.delete(id);reject(new Error(timedOut));},deadline+2000);
            nativeRequests.set(id,(status,body)=>{
                clearTimeout(timer);nativeRequests.delete(id);
                if(status>=200 && status<300) resolve({json:async()=>JSON.parse(body)});
                else reject(new Error(body || "Show host unavailable"));
            });
            window.StudioTablet.request(id,path,data===undefined?"":JSON.stringify(data));
        });
    }
    const response = await fetch("/api/"+path, {
        method: data === undefined ? "GET" : "POST",
        headers: {Authorization:"Bearer "+token,...(data === undefined ? {} : {"Content-Type":"application/json"})},
        body:data === undefined ? undefined : JSON.stringify(data),
        cache:"no-store", signal:AbortSignal.timeout(deadline)
    }).catch(()=>{throw new Error(timedOut);});
    if (!response.ok) throw new Error(await response.text() || response.statusText);
    return response;
}
async function run(key, fn) {
    if (busy.has(key)) return;
    busy.add(key);
    refresh();
    notify("Sending command…");
    try { await fn(); notify("Command accepted",1800); }
    catch (error) { notify(error.message); }
    finally {busy.delete(key); refresh();}
}
function write(key, value, epoch = state?.mixer?.epoch) {
    if (!state?.armed || epoch !== state.mixer.epoch) return;
    return run(key,()=>api("mix",{key,value,epoch}));
}
function toggle(parent, title, key, inverted = false, label, send = false) {
    const get = () => send ? state?.mixer?.send_enabled?.[key]?.enabled : observed(key);
    const active = v => inverted ? !v : Boolean(v);
    const b = button(title,()=>{
        const current = get();
        if (current === undefined || current === null) return;
        if (send) void run(key,()=>api("send",{key,enabled:!current,epoch:state.mixer.epoch}));
        else void write(key,Number(!current));
    },label);
    parent.append(b);
    bindings.push(()=>{
        const v = get();
        b.disabled = !state?.armed || v == null || busy.has(key) || insertBlocks(key)
            || (send && !v && !state?.mixer?.send_enabled?.[key]?.can_enable);
        if (v == null) b.removeAttribute("aria-pressed");
        else b.setAttribute("aria-pressed",active(v));
    });
    return b;
}
function insertBlocks(key) {
    const effect=key.match(/^\/(?:rtn|fxsend)\/([1-4])\//);
    const channelSend=key.match(/^\/ch\/\d+\/mix\/(07|08|09|10)\//);
    const slot=effect?Number(effect[1]):channelSend?Number(channelSend[1])-6:null;
    return slot!==null && observed("/fx/"+slot+"/insert")!==0;
}
function levelBlocked(key) {
    const assignment=state?.mixer?.send_enabled?.[key];
    return insertBlocks(key)
        || (assignment?.mode==="level" && !assignment.enabled && assignment.can_enable);
}
function slider(parent, title, key, format = db, vertical = false, max = 1, step = .001, fxGuard = null) {
    const box=el(vertical?"div":"label",vertical?undefined:title,vertical?"fader":"");
    const cap=el("i",undefined,"cap"), input=el("input"), output=el("output", "—", "db");
    input.type="range"; input.min=0; input.max=max; input.step=step; input.setAttribute("aria-label",title);
    if (vertical) box.append(cap);
    box.append(input); parent.append(box,output);
    let gesture=null, paintedValue;
    let next=null, sending=false, timer=0, invalidGesture=false;
    async function drain(){
        clearTimeout(timer);timer=0;
        if(sending || !next) return;
        const stroke=next; next=null;
        if(stroke.generation!==layoutGeneration || !state?.armed || stroke.epoch!==state.mixer.epoch || !box.isConnected) return;
        sending=true;
        box.setAttribute("aria-busy","true");
        try {
            const request=fxGuard ? {...fxGuard,value:stroke.value,epoch:stroke.epoch}
                                  : {key,value:stroke.value,epoch:stroke.epoch};
            const response = await api(fxGuard?"fx":"mix",request);
            const confirmed = await response.json();
            if (stroke.epoch === state?.mixer?.epoch && Number.isFinite(confirmed)) {
                confirmedFaders.set(key, {value:confirmed, epoch:stroke.epoch, after:syncRequestNumber});
            }
        } catch(error){next=null;invalidGesture=true;notify(error.message);}
        finally {sending=false;box.removeAttribute("aria-busy");if(next)void drain();else refresh();}
    }
    function queue(stroke,value,immediate) {
        if(invalidGesture || !stroke)return;
        next={...stroke,value};
        if(immediate)void drain();
        else if(!sending && !timer)timer=setTimeout(drain,45);
    }
    function paint(value) {
        if (paintedValue === value) return;
        paintedValue = value;
        input.value=String(value);
        const position=1-value/max;
        cap.style.top="calc("+position*100+"% - "+position*23+"px)";
        output.textContent=format(value);
        input.setAttribute("aria-valuetext",output.textContent);
    }
    input.onpointerdown=()=>{invalidGesture=false;gesture={epoch:state?.mixer?.epoch,generation:layoutGeneration};};
    input.onkeydown=()=>{invalidGesture=false;};
    input.onpointercancel=()=>{gesture=null;next=null;invalidGesture=true;refresh();};
    input.oninput=()=>{const value=Number(input.value);paint(value);if(gesture)queue(gesture,value,false);};
    input.onchange=()=>{
        const capture=gesture || {epoch:state?.mixer?.epoch,generation:layoutGeneration};
        const value=Number(input.value);
        gesture=null;
        if(capture.generation===layoutGeneration && box.isConnected) queue(capture,value,true);
    };
    input.onpointerup=()=>{setTimeout(()=>{gesture=null;},0);};
    bindings.push(()=>{
        const v=observed(key);
        const eq=key.match(/^(\/bus\/[12]|\/lr)\/eq\/\d+\//);
        input.disabled=!state?.armed || v===undefined || busy.has(key)
            || (eq && observed(eq[1]+"/eq/mode")!==0)
            || levelBlocked(key);
        const stroke=gesture || next;
        if (input.disabled || (stroke && stroke.epoch!==state?.mixer?.epoch)) {
            if (gesture || next) invalidGesture=true;
            gesture=null; next=null;
        }
        if (!gesture && !next && !sending && !busy.has(key) && v!==undefined) paint(v);
        else if (v===undefined && paintedValue !== undefined) {
            paintedValue=undefined;
            output.textContent="—";
        }
        cap.hidden=v===undefined;
    });
}
function assign(parent, i) {
    if (i >= 9) return;
    const root=strips[i][0];
    ["Main","Aux 1","Aux 2"].forEach((label,mix)=>{
        toggle(parent,label,mix?pathFor(root,mix):root+"/mix/lr",false,stripName(i)+" assign "+label,Boolean(mix));
    });
}
function fineLevel(parent,root) {
    const mix=selected<9?bank:0, key=pathFor(root,mix);
    const destination=selected<9?mixes[mix]:stripName(selected);
    const row=el("div",undefined,"panel"), value=el("output");
    const divisions=mix?160:1023;
    function step(direction) {
        const expected=observed(key);
        if(!Number.isFinite(expected) || !state?.armed)return;
        const target=Math.max(0,Math.min(divisions,Math.round(expected*divisions)+direction))/divisions;
        const epoch=state.mixer.epoch;
        void run(key,()=>api("mix",{key,epoch,expected,value:target}));
    }
    const down=button("−",()=>step(-1),"Lower "+destination+" fader one step");
    const up=button("+",()=>step(1),"Raise "+destination+" fader one step");
    row.append(el("span",destination+" fine level"),down,value,up);parent.append(row);
    bindings.push(()=>{
        const current=observed(key);
        const blocked=!state?.armed || !Number.isFinite(current) || busy.has(key) || levelBlocked(key);
        down.disabled=blocked || current<=0;up.disabled=blocked || current>=1;
        value.textContent=Number.isFinite(current)?db(current,2):"—";
    });
}
function preampControls(parent,channel) {
    const row=el("div",undefined,"panel"), source=el("span"), gain=el("output");
    const minus=button("−0.5 dB",()=>step(-1)), plus=button("+0.5 dB",()=>step(1));
    const power=button("48 V",async()=>{
        if(!shown || power.disabled)return;
        const target={...shown}, generation=layoutGeneration, value=1-target.phantom;
        const label=(value?"Enable":"Turn off")+" 48 V";
        const approved=await confirmDeskAction(label+" phantom power on physical XLR "+target.input+
            "? This changes power to the connected device for all channels using this socket.",label);
        if(!approved || !power.isConnected || power.disabled || generation!==layoutGeneration ||
            shown?.epoch!==target.epoch || shown.input!==target.input || shown.phantom!==target.phantom)return;
        void run("preamp",()=>api("preamp",{epoch:target.epoch,channel,input:target.input,
            control:"phantom",expected:target.phantom,value,confirmed_input:target.input}));
    });
    let shown;
    function step(direction){
        if(!shown || !Number.isFinite(shown.gain))return;
        const {epoch,input,gain:expected}=shown;
        const value=Math.min(144,Math.max(0,Math.round(expected*144)+direction))/144;
        void run("preamp",()=>api("preamp",{epoch,channel,input,control:"gain",expected,value}));
    }
    row.append(source,minus,gain,plus,power);parent.append(row);
    bindings.push(()=>{
        const index=observed(byId(channel,"/ch/")+"/config/insrc");
        const capable=Number.isInteger(index) && index>=0 && index<4;
        const name=index===undefined?"unavailable":capable?"XLR "+(index+1):index<12?"Line "+(index+1):
            index===16?"USB left":index===17?"USB right":"Off";
        source.textContent="Physical source · "+name;
        for(const node of [minus,gain,plus,power])node.hidden=!capable;
        if(!capable){shown=null;return;}
        const input=index+1, key=byId(input,"/headamp/");
        shown={epoch:state.mixer.epoch,input,gain:observed(key+"/gain"),phantom:observed(key+"/phantom")};
        const canAdjust=state.armed && !busy.has("preamp");
        gain.textContent=Number.isFinite(shown.gain)?(shown.gain*72-12).toFixed(1)+" dB":"—";
        minus.disabled=!canAdjust || !Number.isFinite(shown.gain) || shown.gain<=0;
        plus.disabled=!canAdjust || !Number.isFinite(shown.gain) || shown.gain>=1;
        power.disabled=!canAdjust || ![0,1].includes(shown.phantom);
        power.textContent="48 V · "+(shown.phantom===1?"on":shown.phantom===0?"off":"unknown");
        power.setAttribute("aria-pressed",shown.phantom===1);
    });
}
function selectPage(n){
    page=(n+pages.length)%pages.length;
    transportBindings=undefined;
    build();
}
function build() {
    layoutGeneration++;
    bindings=[];
    meterBindings=[];
    const deck=$("deck"), bankBar=$("banks"), nav=$("pages"), assignBar=$("assign");
    const armKey=$("arm");
    [deck,bankBar,nav,assignBar].forEach(n=>n.replaceChildren());
    assign(assignBar,selected);
    mixes.forEach((m,i)=>{
        const b=button(m,()=>{bank=i;build();});
        b.setAttribute("aria-pressed",bank===i); bankBar.append(b);
    });
    bankBar.append(armKey);
    pages.forEach((p,i)=>{
        const b=button(p,()=>selectPage(i)); b.setAttribute("aria-pressed",page===i); nav.append(b);
    });
    nav.append(button("‹",()=>selectPage(page-1),"Previous page"),button("›",()=>selectPage(page+1),"Next page"));
    strips.forEach(([root],i)=>{
        const strip=el("section",undefined,"strip "+(i<9?"input":i<11?"aux":i<15?"fx":"master"));
        const pick=button(stripName(i),()=>{
            selected=i;
            if(!hasAnalyzerAlert(root)){build();return;}
            selectPage(2);
            const screen=$("screen"), reading=screen.querySelector(".feedback-reading");
            reading.focus({preventScroll:true});
            screen.scrollTop+=reading.getBoundingClientRect().top-screen.getBoundingClientRect().top;
        });
        pick.className="pick"; pick.setAttribute("aria-pressed",i===selected);
        bindings.push(()=>{
            const name=stripName(i), alert=hasAnalyzerAlert(root);
            pick.textContent=name;
            pick.classList.toggle("analyzer-alert",alert);
            pick.setAttribute("aria-label",alert?name+" · analyzer alert, open EQ":name);
        });
        const dest=i<9?mixes[bank]:i<11?"BUS MASTER":i<15?"FX RETURN":"MASTER";
        const scope=el("span",dest,"scope");
        strip.append(pick,scope);
        if(i<9 && bank) {
            const tapKey=pathFor(root,bank).replace("/level","/tap");
            bindings.push(()=>{
                const tap=observed(tapKey);
                scope.textContent=dest+" · "+(["IN","PRE EQ","POST EQ","PRE","POST","GRP"][tap]??"—");
                scope.title=dest+" · "+(sendTaps[tap]??"tap unavailable");
            });
        }
        const routes=el("div",undefined,"strip-assign"); assign(routes,i);strip.append(routes);
        const row=el("div",undefined,"sendrow");
        if(i<9) row.append(button("SENDS",()=>{selected=i;selectPage(6);},stripName(i)+" sends"));
        strip.append(row);
        toggle(strip,i<9?"MUTE ALL":"MUTE",root+"/mix/on",true,stripName(i)+" global mute").className="lamp";
        slider(strip,stripName(i)+" "+dest+" level",pathFor(root,i<9?bank:0),db,true);
        const meter=el("i",undefined,"strip-meter");
        strip.querySelector(".fader").append(meter);
        meterBindings.push(()=>{
            const levels=state?.mixer?.meters["/meters/1"]?.value;
            const index=i<9?Number(root.slice(4))-1:i<11?26+i-9:i<15?18+2*(i-11):36;
            const level=levels && (i>=11?Math.max(levels[index],levels[index+1]):levels[index]);
            meter.hidden=!Number.isFinite(level);
            if(!meter.hidden){
                meter.style.setProperty("--level",Math.min(100,Math.max(0,(level+60)*100/60))+"%");
                meter.classList.toggle("clip",level>=0);
            }
        });
        deck.append(strip);
    });
    buildScreen();
    refresh();
}
function action(parent,label,id,allowed) {
    const b=button(label,()=>run("show",()=>api("show",{id:crypto.randomUUID(),revision:state.revision,action:id})));
    bindings.push(()=>{b.disabled=!state?.show || busy.has("show") || (allowed && !allowed());});parent.append(b);
    return b;
}
function buildScreen() {
    const s=$("screen"), root=strips[selected][0];
    if(page===0 && transportBindings){
        bindings.push(...transportBindings);
        return;
    }
    s.replaceChildren();
    s.classList.remove("operator-detail");
    s.scrollTop=0;
    if(page===0){
        const start=bindings.length;
        const transport=el("section",undefined,"now-playing");
        const name=el("h2"), detail=el("p"), playback=el("p"), controls=el("div",undefined,"panel");
        transport.append(name,detail,playback,controls);
        s.append(transport);
        bindings.push(()=>{
            const show=state?.show, slots=show?.slots||[], live=slots.find(x=>x.is_current);
            const queued=slots.find(x=>x.request_id && x.request_id===show?.next_request);
            const current=live || queued;
            const nextNames=show?.next_singers?.join(", ");
            name.textContent=current?.singer_name || nextNames || slots[0]?.singer_name || (show?"Rotation is empty":"Waiting for the Auto-KJ host");
            const song=live?live.song_title:show?.next_title || queued?.song_title;
            detail.textContent=[song,current?.song_artist,live && nextNames?"Next: "+nextNames:""].filter(Boolean).join(" · ");
            if(!show)playback.textContent="";
            else if(state.playback.state==="stopped")playback.textContent=show.next_request?"Ready to play":"Stopped";
            else playback.textContent=state.playback.state+" · "+time(state.playback.elapsed)+
                (state.playback.duration>0?" elapsed · "+time(Math.max(0,state.playback.duration-state.playback.elapsed))+" remaining":"");
            if(show?.break_status?.playing){
                name.textContent="Break music";
                detail.textContent=show.break_status.title;
                playback.textContent=show.break_status.automix?"Automix · "+show.break_status.phase:"Playing";
            }
        });
        action(controls,"Play","play",()=>state.playback.state==="stopped" && !state.show.active_request && Boolean(state.show.next_request));
        action(controls,"Pause","pause",()=>state.playback.state==="playing");
        action(controls,"Resume","resume",()=>state.playback.state==="paused");
        action(controls,"Stop","stop",()=>state.playback.state!=="stopped" || Boolean(state.show.active_request));
        action(controls,"Next singer","next",()=>Boolean(state.show.active_request || state.show.next_request));
        action(controls,"Play break","break_play",()=>state.playback.state==="stopped" && !state.show.break_status?.playing);
        action(controls,"Stop break","break_stop",()=>state.show.break_status?.playing || state.show.break_status?.automix);
        const skipBreak=action(controls,"Skip break","break_skip",()=>state.show.break_status?.playing && state.show.break_status?.skip_allowed);
        bindings.push(()=>{skipBreak.textContent=state?.show?.break_status?.skip_allowed===false?"Skip break (Full)":"Skip break";});
        const updateBreak=renderOperator(s, action => api("operator",action).then(r=>r.json()));
        bindings.push(()=>updateBreak(state?.show?.break_status?.revision));
        transportBindings=bindings.slice(start);
        return;
    }
    const title=el("h2");bindings.push(()=>{title.textContent=pages[page]+" · "+stripName(selected);});s.append(title);
    const panel=el("div",undefined,"panel");s.append(panel);
    const pct=n=>Math.round(n*100)+"%";
    if(page===1){
        assign(panel,selected);
        fineLevel(s,root);
        slider(panel,"Pan",root+"/mix/pan",n=>Math.round(n*200-100).toString());
        const sockets=el("p");s.append(sockets);
        bindings.push(()=>{
            const taps=["Analog in","Analog in + mute","Input","Input + mute","Pre EQ","Pre EQ + mute",
                "Post EQ","Post EQ + mute","Pre fader","Pre fader + mute","Post fader"];
            sockets.textContent=[1,2].map(number=>{
                const path=byId(number,"/routing/aux/");
                return "Aux out "+number+" ← "+socketSource(observed(path+"/src"))+
                    " ("+(taps[observed(path+"/pos")]??"tap unavailable")+")";
            }).join(" · ");
        });
        if(selected<9){
            const current=Number(root.slice(4)), pairBase=current%2?current:current-1;
            const label=el("label","Strip input"),source=el("select");
            for(let i=1;i<=12;i++) source.add(new Option("XR12 input "+i,i));
            source.value=String(current);source.disabled=!layoutRig;
            source.onchange=()=>{
                const ids=strips.slice(0,9).map(([path])=>Number(path.slice(4)));
                const next=Number(source.value),other=ids.indexOf(next);
                if(other>=0)ids[other]=current;
                ids[selected]=next;applyInputs(ids);
                localStorage.setItem("studio.inputs."+layoutRig,JSON.stringify(ids));build();
            };
            label.append(source);panel.append(label);
            preampControls(s,current);
            const link=el("p"); s.append(link);
            bindings.push(()=>{
                const linked=observed("/config/chlink/"+pairBase+"-"+(pairBase+1));
                link.textContent=linked===1?"Stereo link · inputs "+pairBase+" and "+(pairBase+1):"";
            });
        }
        const flat=button("High contrast",()=>{
            const on=document.body.classList.toggle("flat");
            localStorage.setItem("studio.flat",on?"1":"0");flat.setAttribute("aria-pressed",on);
        });flat.setAttribute("aria-pressed",document.body.classList.contains("flat"));s.append(flat);
    } else if(page===2){
        toggle(panel,"EQ enabled",root+"/eq/on");
        const analyzer=el("p",undefined,"feedback-reading"), axis=el("div",undefined,"axis"), axisCaption=el("small");
        analyzer.tabIndex=-1;
        axis.setAttribute("aria-hidden","true");
        s.append(analyzer,axis,axisCaption);
        let previousAxis;
        bindings.push(()=>{
            const r=state?.mixer?.analyzer;
            axis.hidden=axisCaption.hidden=!r;
            if(!r){analyzer.textContent="Feedback monitor · waiting for fresh analyzer data";return;}
            const src=r.source<12?"Input "+(r.source+1):r.source===31?"Main LR":r.source===32?"monitor output":
                r.source>=17 && r.source<=20?"FX return "+(r.source-16):
                r.source>=21 && r.source<=26?"Bus "+(r.source-20):"XR12 source "+r.source;
            const peaks=r.candidates??[], bins=peaks.map(p=>p.bin);
            const mapped=state.mixer.frequency_map??[];
            const bracket=p=>p?Math.floor(p.low_hz)+"–"+Math.ceil(p.high_hz)+" Hz":"";
            const peakRange=bracket(mapped.find(p=>p.bin===r.bin));
            const result=r.competing?"Several rising peaks":r.bin!=null?"Rising narrow peak · bin "+(r.bin+1)+(peakRange?" · "+peakRange:""):"No rising narrow peak";
            analyzer.textContent="Monitoring "+src+" · "+(r.post?"post EQ":"pre EQ")+" · "+result;
            const axisKey=JSON.stringify([bins,mapped]);
            if(previousAxis!==axisKey){
                previousAxis=axisKey;
                const ticks=[];
                let previousTick=-100;
                for(const p of mapped){
                    if(p.bin-previousTick<18)continue;
                    previousTick=p.bin;
                    const tick=el("span",Math.round(p.tested_low_hz)+" Hz","cal-tick");
                    tick.style.left=(p.bin+.5)+"%";
                    tick.title="Test tone in bin "+(p.bin+1)+" · measured bracket "+bracket(p);
                    ticks.push(tick);
                }
                axis.classList.toggle("measured",ticks.length>0);
                axis.replaceChildren(...ticks,...bins.map(bin=>{
                    const pin=el("i",undefined,"pin");
                    pin.style.left=((bin+.5)/100*100)+"%";
                    return pin;
                }));
            }
            const peaksText=peaks.map(p=>{
                const range=bracket(mapped.find(m=>m.bin===p.bin));
                const prominence="+"+p.prominence_db.toFixed(1)+" dB over neighbors";
                return p.bin===r.bin?prominence:"bin "+(p.bin+1)+(range?" · "+range:"")+" ("+prominence+")";
            }).join(", ");
            const cadence=r.mean_interval_ms==null?"": " · "+Math.round(r.mean_interval_ms)+" ms/frame";
            const windowText=Number.isFinite(r.window_ms)?" · "+(r.window_ms/1000).toFixed(1)+" s observed":"";
            axisCaption.textContent="One shared analyzer tap · bins 1–100 · monitor only"+windowText+cadence+(mapped.length?" · "+mapped.length+" measured "+(mapped.length===1?"bin":"bins")+" · ticks show test tones":"")+(peaksText?" · "+peaksText:"");
        });
        const bands=selected===15 || (selected>=9 && selected<11)?6:4;
        for(let i=1;i<=bands;i++){
            const row=el("div",undefined,"panel"); s.append(row);
            const base=root+"/eq/"+i;
            enumerated(row,"Filter",base+"/type",["Low cut","Low shelf","PEQ","Vintage EQ","High shelf","High cut"]);
            slider(row,"Band "+i+" · Hz",base+"/f",n=>Math.round(20*Math.pow(1000,n))+" Hz");
            slider(row,"Gain",base+"/g",n=>(n*30-15).toFixed(1)+" dB");
            slider(row,"Q",base+"/q",n=>(10*Math.pow(.03,n)).toFixed(2));
        }
    } else if(page===3){
        if(root.startsWith("/rtn/")) {
            s.append(el("p","FX returns have no channel compressor. Select an input, an aux master or Main LR."));
            return;
        }
        const reduction=el("output"), mode=el("span"), meterRow=el("div",undefined,"panel");
        const index=root==="/lr"?38:root.startsWith("/bus/")?31+Number(root.slice(5)):15+Number(root.slice(4));
        meterRow.append(mode,reduction);s.append(meterRow);
        bindings.push(()=>{
            mode.textContent=["Compressor","Expander"][observed(root+"/dyn/mode")]??"Dynamics";
        });
        meterBindings.push(()=>{
            const sample=state?.mixer?.connected?state.mixer.meters["/meters/6"]?.value?.[index]:undefined;
            reduction.textContent="Gain reduction · "+(Number.isFinite(sample)?sample.toFixed(1)+" dB":"—");
        });
        toggle(panel,"Dynamics enabled",root+"/dyn/on");
        slider(panel,"Threshold",root+"/dyn/thr",n=>(n*60-60).toFixed(1)+" dB");
        enumerated(panel,"Ratio",root+"/dyn/ratio",["1.1","1.3","1.5","2","2.5","3","4","5","7","10","20","100"].map(n=>n+":1"));
        slider(panel,"Attack",root+"/dyn/attack",n=>Math.round(n*120)+" ms");
        slider(panel,"Release",root+"/dyn/release",n=>Math.round(5*Math.pow(800,n))+" ms");
        slider(panel,"Make-up",root+"/dyn/mgain",n=>(n*24).toFixed(1)+" dB");
        slider(panel,"Mix",root+"/dyn/mix",pct);
    } else if(page===4 || page===5){
        for(let i=1;i<=4;i++){
            const b=button("FX "+i,()=>{fx=i;build();}); b.setAttribute("aria-pressed",fx===i);panel.append(b);
        }
        effectPanel(s,panel,root);
    } else if(page===6){
        if(selected>=9){s.append(el("p","Select an input to see its sends."));return;}
        for(let i=1;i<mixes.length;i++){
            const row=el("div",undefined,"panel");s.append(row);
            toggle(row,mixes[i],pathFor(root,i),false,stripName(selected)+" enable "+mixes[i],true);
            slider(row,mixes[i]+" send",pathFor(root,i));
            const tap=el("small");row.append(tap);
            bindings.push(()=>{const id=observed(pathFor(root,i).replace("/level","/tap"));tap.textContent=sendTaps[id]||"";});
        }
    }
}
function enumerated(parent,title,key,labels){
    const label=el("label",title), choice=el("select");
    labels.forEach((name,i)=>choice.add(new Option(name,i)));
    choice.setAttribute("aria-label",title);label.append(choice);parent.append(label);
    bindings.push(()=>{
        const v=observed(key), eq=key.match(/^(\/bus\/[12]|\/lr)\/eq\//);
        choice.disabled=!state?.armed || v===undefined || busy.has(key) || insertBlocks(key) || (eq && observed(eq[1]+"/eq/mode")!==0);
        choice.value=v===undefined?"":String(v);
    });
    choice.onchange=()=>write(key,Number(choice.value));
}
function guardedChoice(parent,title,key,options,precondition=()=>observed(key)){
    const label=el("label",title), choice=el("select"), apply=button("Apply",commit);
    for(const [value,name] of options) choice.add(new Option(name,String(value)));
    let pending=null, previous;
    function commit(){
        const expected=precondition(), next=Number(choice.value);
        if(expected==null || !state?.armed)return;
        if(pending && pending.next===next && pending.epoch===state.mixer.epoch &&
           JSON.stringify(pending.expected)===JSON.stringify(expected)){
            const {epoch}=pending;pending=null;
            void run(key,()=>api("mix",{key,value:next,expected,epoch}));
        } else pending={expected,next,epoch:state.mixer.epoch};
        refresh();
    }
    choice.setAttribute("aria-label",title);choice.onchange=()=>{pending=null;refresh();};
    label.append(choice);parent.append(label,apply);
    bindings.push(()=>{
        const v=observed(key);
        if(v!==previous || (pending && state?.mixer.epoch!==pending.epoch)){choice.value=String(v);previous=v;pending=null;}
        choice.disabled=!state?.armed || v===undefined || busy.has(key);
        apply.disabled=choice.disabled || !choice.value || Number(choice.value)===v || precondition()==null;
        apply.textContent=pending?"Confirm "+title:"Apply "+title;
    });
}
function effectPanel(screen, panel,root){
    const slot=fx, effect=state?.mixer.effects?.[slot], base="/fx/"+slot;
    renderedFx=effect?.algorithm;
    const info=el("p");screen.append(info);
    bindings.push(()=>{
        const kind=observed(base+"/type"), insert=observed(base+"/insert");
        info.textContent=kind==null?"Waiting for FX slot readback":"Slot "+slot+" · "+algorithms[kind]+" · "+(insert===1?"Insert":insert===0?"Send/return":"Routing unknown");
    });
    guardedChoice(screen,"algorithm",base+"/type",algorithms.map((name,i)=>[i,name]));
    guardedChoice(screen,"slot routing",base+"/insert",[[0,"Send/return"],[1,"Insert"]]);
    if(page===4){
        if(effect?.algorithm!==30){screen.append(el("p","Select a slot loaded with LIM, or explicitly replace its algorithm."));return;}
        const target=el("p");screen.append(target);
        const bus=root==="/lr", choices=[[0,"None"]];
        for(let i=1;i<=(bus?4:8);i++)choices.push([i,bus?"FX "+i:"FX "+Math.ceil(i/2)+(i%2?" A":" B")]);
        guardedChoice(screen,"insert target",root+"/insert/fxslot",choices);
        guardedChoice(screen,"insert switch",root+"/insert/on",[[0,"Bypassed"],[1,"In circuit"]],()=>{
            const on=observed(root+"/insert/on"), targetSlot=observed(root+"/insert/fxslot");
            if(on===1)return on;
            const slotIndex=bus?targetSlot:Math.ceil(targetSlot/2);
            return on===0 && slotIndex===slot && observed(base+"/insert")===1 ?
                {value:0,fxslot:targetSlot,algorithm:effect.algorithm}:null;
        });
        bindings.push(()=>{
            const mapping=observed(root+"/insert/fxslot"), linked=bus?mapping:Math.ceil(mapping/2);
            target.textContent=observed(root+"/insert/on")===1 && linked===slot && observed(base+"/insert")===1 ?
                "Limiter inserted on "+stripName(selected):"Selected strip has no confirmed limiter in circuit";
        });
    }else{
        slider(screen,"FX send master","/fxsend/"+slot+"/mix/fader");
        slider(screen,"Stereo return","/rtn/"+slot+"/mix/fader");
        const mixSends=button("Mix FX sends",()=>{bank=slot+2;build();});
        panel.append(mixSends);
        bindings.push(()=>{mixSends.disabled=observed(base+"/insert")!==0;});
        for(const bus of [1,2]){
            const row=el("div",undefined,"panel"), key=pathFor("/rtn/"+slot,bus);
            const name="FX "+slot+" return to Aux "+bus;
            toggle(row,"Aux "+bus,key,false,"Enable "+name,true);
            slider(row,name,key);screen.append(row);
        }
    }
    effect?.parameters.forEach((param,i)=>{
        const key=base+"/par/"+String(i+1).padStart(2,"0");
        const format=n=>{
            const value=param.steps<0 && param.min>0?param.min*Math.pow(param.max/param.min,n):param.min+(param.max-param.min)*n;
            return Number(value.toFixed(2))+" "+param.unit;
        };
        slider(screen,param.name,key,format,false,1,1/Math.max(1,Math.abs(param.steps)-1),{slot,algorithm:effect.algorithm,index:i+1});
    });
}
function refresh(){
    if((page===4 || page===5) && renderedFx!==state?.mixer.effects?.[fx]?.algorithm){build();return;}
    $("selection").textContent=stripName(selected);
    const mix=state?.mixer;
    const status=(mix?.connected?(mix.identity?.name || "XR12")+" connected":"XR12 offline")+" · "+(state?.show?"Host connected":"Host offline");
    if(status!==statusText){$("status").textContent=status;statusText=status;}
    $("arm").disabled=!mix?.connected || busy.has("arm");
    $("arm").textContent=state?.armed?"Disable mixer control":"Enable mixer control";
    for(const bind of bindings)bind();
    refreshMeters();
}
function refreshMeters() {
    for(const bind of meterBindings)bind();
    const mix=state?.mixer;
    const sample=mix?.connected && mix.meters["/meters/1"]?.value;
    const now=performance.now();
    for(const [id,index] of [["left",36],["right",37]]){
        const level=sample?.[index], gauge=$(id);
        gauge.classList.toggle("off",!Number.isFinite(level));
        const number=Number.isFinite(level)?level.toFixed(1)+" dBFS":"—";
        gauge.firstElementChild.textContent=(id==="left"?"L ":"R ")+number;
        gauge.setAttribute("aria-label",id+" program level "+number);
        if(!Number.isFinite(level)){
            programMeters.delete(id);
            gauge.classList.remove("clip");
            continue;
        }
        const previous=programMeters.get(id), amplitude=Math.pow(10,level/20);
        const weight=previous?.epoch===mix.epoch ? Math.exp(-(now-previous.time)/300) : 0;
        const average=amplitude*(1-weight)+(previous?.average??0)*weight;
        const clipUntil=level>=0?now+1000:(weight?previous.clipUntil:0);
        programMeters.set(id,{epoch:mix.epoch,time:now,average,clipUntil});
        const fraction=Math.min(1,Math.max(0,(20*Math.log10(average)+55)/55));
        gauge.style.setProperty("--needle",(-60+fraction*110)+"deg");
        gauge.classList.toggle("clip",now<clipUntil);
    }
}
$("arm").onclick=()=>run("arm",()=>api("arm",{epoch:state.mixer.epoch,enabled:!state.armed}));
let visibilityGeneration=0, syncTimer, syncing=false;
document.addEventListener("visibilitychange",()=>{
    confirmedFaders.clear();
    visibilityGeneration++;
    layoutGeneration++;
    state=null;
    clearTimeout(syncTimer);
    refresh();
    if(!document.hidden && !syncing)void poll();
});
async function poll(){
    if(document.hidden || syncing)return;
    const generation=visibilityGeneration;
    const requestNumber=++syncRequestNumber;
    syncing=true;
    try {
        if(!token && !window.StudioTablet) throw new Error("Open a pairing link from Auto-KJ Settings.");
        const incoming=await (await api("sync",{cursor:state?.cursor??null})).json();
        if(generation!==visibilityGeneration)return;
        if(incoming.delta && state){
            const previous=state.mixer, changes=incoming.mixer;
            for(const key of changes.removed || [])delete previous.values[key];
            Object.assign(previous.values,changes.values);
            incoming.mixer={...previous,...changes,values:previous.values};
            if(!Object.hasOwn(incoming,"show"))incoming.show=state.show;
        } else incoming.mixer.meters={};
        state=incoming;
        // An acknowledgement is newer than any state request already in flight.
        // The next request sees the mixer's current value, including another operator's edit.
        for (const [key, confirmed] of confirmedFaders) {
            if (requestNumber > confirmed.after || confirmed.epoch !== state.mixer.epoch) {
                confirmedFaders.delete(key);
            }
        }
        if(lastSyncError && $("notice").textContent===lastSyncError)notify("");
        lastSyncError="";
        restoreLayout(state.mixer.identity);
    } catch(error){
        if(generation===visibilityGeneration){state=null;lastSyncError=error.message;notify(lastSyncError);}
    } finally {
        syncing=false;refresh();
        if(!document.hidden)syncTimer=setTimeout(poll,generation!==visibilityGeneration?0:state?.mixer?.connected?100:450);
    }
}
async function pollMeters(){
    if(state?.mixer?.connected && !document.hidden) {
        try {
            const incoming=await (await api("meters")).json();
            if(incoming.epoch===state?.mixer?.epoch)state.mixer.meters=incoming.meters;
        }catch{if(state)state.mixer.meters={};}
        refreshMeters();
    }
    setTimeout(pollMeters,state?.mixer?.connected && !document.hidden?100:500);
}
build();void poll();void pollMeters();

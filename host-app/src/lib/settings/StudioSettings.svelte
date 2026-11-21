<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import QRCode from "qrcode";
  import { onMount } from "svelte";

  let mixer = $state(localStorage.getItem("studio-mixer-address") || "");
  type Pairing = {id:string;tablet_url:string;certificate_sha256:string};
  let pairing = $state<Pairing | null>(null);
  let stations = $state<string[]>([]);
  async function updateStations(revoke?:string) {
    try {
      stations=await invoke<string[]>("studio_stations",{revoke});
      if(revoke===pairing?.id){pairing=null;qr="";}
    }catch(error){message=String(error);}
  }
  onMount(()=>{void updateStations();});
  let qr = $state("");
  let message = $state("");
  let busy = $state(false);

  async function pair() {
    busy = true;
    message = "";
    try {
      const result = await invoke<Pairing>("pair_studio_console", {mixer});
      const endpoint = new URL(result.tablet_url);
      const appLink = "autokj-studio://pair?address="+encodeURIComponent(endpoint.host)+endpoint.hash;
      qr = await QRCode.toDataURL(appLink, {width:240,margin:2});
      pairing = result;
      await updateStations();
      localStorage.setItem("studio-mixer-address",mixer.trim());
    } catch (error) {message = String(error);}
    finally {busy = false;}
  }

  async function open() {
    busy = true;
    try {await invoke("open_studio_console");await updateStations();} catch(error) {message = String(error);}
    finally {busy = false;}
  }

  async function stop() {
    try {
      await invoke("stop_studio_console");
      pairing = null; qr = "";stations=[]; message = "Console stopped. All operator pairings revoked.";
    } catch(error) {message = String(error);}
  }
</script>

<section class="settings-card span-two" id="card-studio-console">
  <h2>XR12 studio console</h2>
  <p class="description">Connect to the mixer on the venue network. Each station starts with mixer controls read-only.</p>
  <label>XR12 IPv4 address <input bind:value={mixer} placeholder="192.168.1.1" /></label>
  <button class="secondary-btn" disabled={!mixer.trim() || busy} onclick={pair}>{pairing ? "Pair another station" : "Connect and pair"}</button>
  <button class="secondary-btn" disabled={busy} onclick={open}>Open desktop console</button>
  <button class="secondary-btn" onclick={stop}>Stop console and revoke pairings</button>
  {#if pairing}
    <p>Station {pairing.id.slice(0,8)}</p>
    <p>Scan on the operator tablet. This QR grants control of the show.</p>
    <img src={qr} alt="Private operator pairing QR" width="240" height="240" />
    <details><summary>Manual tablet pairing</summary>
      <label>Paste into the Studio Console app <input readonly type="password" value={pairing.tablet_url} onfocus={(e)=>e.currentTarget.select()} /></label>
      <p>Host certificate SHA-256</p><code>{pairing.certificate_sha256}</code>
    </details>
  {/if}
  {#each stations as station (station)}
    <button class="secondary-btn" onclick={()=>updateStations(station)}>Revoke station {station.slice(0,8)}</button>
  {/each}
  {#if message}<p role="status">{message}</p>{/if}
</section>

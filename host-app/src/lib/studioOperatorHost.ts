import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { InboxRequest } from "./inbox";
import type { LineView, SingerView, SingerQueueEntry } from "./types";
import { PADS, stopAllSamples, triggerSoundboardFx } from "./dj/soundboardAudio";
import { KEY_RANGE } from "./shortcutActions";

interface Host {
  inbox(): InboxRequest[];
  approve(card: InboxRequest): Promise<void>;
  reject(card: InboxRequest): Promise<void>;
  accepting(): boolean;
  setAccepting(on: boolean): Promise<void>;
  refresh(): Promise<void>;
  blocked(): string | null;
  knobs(pitch: number, tempo: number): void;
  stop(): Promise<void>;
  stopGuarded(operation: () => Promise<void>): Promise<void>;
  startGuarded(request: string, generation: number, callId: string): Promise<void>;
}
interface Action {
  type: string;
  singer?: string;
  partners?: string[];
  name?: string;
  slot?: number;
  from?: number;
  target?: string;
  paused?: boolean;
  request?: string;
  first?: string;
  second?: string;
  key?: string;
  approve?: boolean;
  semitones?: number;
  track?: number;
  pad?: string;
  enabled?: boolean;
  confirmed?: boolean;
  override_duplicate?: boolean;
  confirm_group?: boolean;
  command?: "play" | "stop" | "next";
  current?: string | null;
  next?: string | null;
  generation?: number;
  loaded?: boolean;
}
interface Call {
  id: string;
  deadline: number;
  action: Action;
  track?: { filepath: string; artist: string; title: string };
}
type Playback = { pitch: number; tempo: number; state: string; load_gen: number };

async function overview(host: Host) {
  const [line, singers, audio] = await Promise.all([
    invoke<LineView>("get_line_view"),
    invoke<SingerView[]>("get_singers"),
    invoke<Playback>("audio_get_info"),
  ]);
  return {
    singers: singers.map(({ id, name, is_paused, is_idle }) => ({
      id,
      name,
      paused: is_paused,
      idle: !!is_idle,
    })),
    slots: line.slots.map(({ singer_ids, names, song_title, is_current }) => ({
      singers: singer_ids,
      names,
      title: song_title,
      current: is_current,
    })),
    inbox: host
      .inbox()
      .map(({ key, singer_name, title, artist }) => ({ key, singer: singer_name, title, artist })),
    accepting: host.accepting(),
    pitch: audio.pitch,
    track: audio.load_gen,
    playback: audio.state,
    pads: PADS.map(({ id, label }) => ({ id, label })),
    blocked: host.blocked(),
  };
}

export async function handleStudioAction(call: Call, host: Host): Promise<unknown> {
  const action = call.action;
  if (call.deadline < Date.now()) throw Error("Operator request expired before execution");
  if (action.type === "overview") return overview(host);
  if (action.type === "queue") {
    const songs = await invoke<SingerQueueEntry[]>("get_singer_queue", { singerIds: [action.singer] });
    return {
      songs: songs.map((song) => ({
        id: song.request_id,
        title: song.song_title,
        artist: song.song_artist,
        status: song.status,
        is_group: song.is_group,
        group_ready: song.group_ready,
        group_waiting_on: song.group_waiting_on,
      })),
    };
  }
  const blocked = host.blocked();
  // A halted seat can still stop its current performance through the booth transport.
  const stoppingCurrentSong = action.type === "transport" && action.command === "stop";
  if (blocked && !stoppingCurrentSong) throw Error(blocked);
  let result: unknown;
  switch (action.type) {
    case "add_singer":
      result = await invoke("add_singer", { name: action.name });
      break;
    case "rename_singer":
      result = await invoke("rename_singer", { singerId: action.singer, newName: action.name });
      break;
    case "move_singer":
      result = await invoke("move_singer", {
        singerId: action.singer,
        toSlot: action.slot,
        expectedFrom: action.from,
        expectedTarget: action.target,
      });
      break;
    case "remove_singer":
      result = await invoke("delete_singer", { singerId: action.singer });
      break;
    case "set_paused": {
      const singer = (await invoke<SingerView[]>("get_singers")).find((row) => row.id === action.singer);
      if (!singer) throw Error("Singer has left this show");
      result = await invoke(action.paused ? "pause_singer" : "unpause_singer", {
        singerName: singer.name,
        expectedSingerId: singer.id,
      });
      break;
    }
    case "assign":
    case "replace": {
      if (!call.track) throw Error("Catalog song unavailable");
      const args = {
        songFilepath: call.track.filepath,
        songTitle: call.track.title,
        songArtist: call.track.artist,
        ...(action.override_duplicate === true ? { forceDuplicate: true } : {}),
      };
      if (action.type === "replace") {
        result = await invoke("replace_request_song", {
          ...args,
          oldRequestId: action.request,
          expectedSingerId: action.singer,
        });
      } else if (action.partners?.length) {
        result = await invoke("add_group_song_request", {
          ...args,
          singerIds: [action.singer, ...action.partners],
        });
      } else {
        const singer = (await invoke<SingerView[]>("get_singers")).find((row) => row.id === action.singer);
        if (!singer) throw Error("Singer has left this show");
        result = await invoke("add_song_request", {
          ...args,
          singerName: singer.name,
          expectedSingerId: singer.id,
        });
      }
      break;
    }
    case "remove_song":
      result = await invoke("delete_request", {
        requestId: action.request,
        expectedGroup: action.confirm_group === true,
      });
      break;
    case "swap":
      result = await invoke("swap_queue_entries", {
        singerId: action.singer,
        requestA: action.first,
        requestB: action.second,
      });
      break;
    case "review": {
      const card = host.inbox().find((row) => row.key === action.key);
      if (!card) throw Error("Request already resolved");
      if (action.approve) await host.approve(card);
      else await host.reject(card);
      if (host.inbox().some((row) => row.key === action.key))
        throw Error("Request remains in the host inbox");
      // The desktop can resolve an approval by declining a blocked or duplicate song.
      result = "Request resolved by the show host";
      break;
    }
    case "key": {
      const pitch = action.semitones;
      if (
        typeof pitch !== "number" ||
        !Number.isInteger(pitch) ||
        pitch < KEY_RANGE.min ||
        pitch > KEY_RANGE.max
      )
        throw Error("Key is outside the host range");
      const info = await invoke<Playback>("audio_get_info");
      if (info.state === "stopped") throw Error("No rehearsal track is loaded");
      if (!action.track || info.load_gen !== action.track) throw Error("The track changed. Refresh the key.");
      await invoke("audio_set_pitch", { semitones: pitch, expectedGen: action.track });
      const applied = await invoke<Playback>("audio_get_info");
      if (applied.load_gen === action.track && applied.state !== "stopped") host.knobs(pitch, applied.tempo);
      break;
    }
    case "accept_requests":
      await host.setAccepting(!!action.enabled);
      if (host.accepting() !== action.enabled) throw Error("Host did not update request availability");
      break;
    case "pad":
      if (!PADS.some((p) => p.id === action.pad) || !(await triggerSoundboardFx(action.pad!)))
        throw Error("Soundboard pad unavailable");
      break;
    case "new_show": {
      if (!action.confirmed) throw Error("Confirm clearing the current singer list");
      const playback = await invoke<Playback>("audio_get_info");
      if (playback.state !== "stopped") throw Error("Stop the rehearsal track before clearing the line");
      result = await invoke("start_new_show");
      break;
    }
    case "transport": {
      const { generation, command, current, loaded } = action;
      if (command === "play") {
        if (generation === undefined || loaded || current || !action.next)
          throw Error("The projected singer is unavailable.");
        await host.startGuarded(action.next, generation, call.id);
        break;
      }
      if (generation === undefined || (command !== "stop" && command !== "next"))
        throw Error("Unrecognized guarded transport request");
      let next = action.next;
      if (!loaded && !current && (command === "stop" || !next))
        throw Error("There is no performance to control");
      await host.stopGuarded(async () => {
        if (current) {
          const completion = await invoke<{ next: string | null }>("complete_current", {
            expectedRequest: current,
            expectedGeneration: generation,
            studioCall: call.id,
            checkSuccessor: command === "next",
            expectedSuccessor: next ?? null,
          });
          next = completion.next;
        }
        if (loaded && !(await invoke<boolean>("audio_stop_if", { gen: generation })))
          throw Error(
            current
              ? "The observed turn completed. Playback has changed; refresh the console."
              : "The track changed before stopping",
          );
      });
      if (command === "next" && next) {
        try {
          await host.startGuarded(next, generation, call.id);
        } catch (error) {
          if (current) throw Error(`The previous performance completed. ${error instanceof Error ? error.message : error}`);
          throw error;
        }
      }
      break;
    }
    case "end_performances": {
      if (!action.confirmed) throw Error("Confirm ending performances");
      await host.setAccepting(false);
      if (host.accepting()) throw Error("Requests are still open; performances continue");
      await host.stop();
      const playback = await invoke<Playback>("audio_get_info");
      if (playback.state !== "stopped") throw Error("Requests closed; karaoke has not confirmed Stop");
      await invoke("break_wave_stop");
      stopAllSamples();
      result = "Requests closed and show playback stopped";
      break;
    }
    default:
      throw Error("Unknown operator action");
  }
  await host.refresh();
  return result ?? true;
}

export function listenStudioOperator(host: Host) {
  // The host window completes each action before beginning the next one.
  let current = Promise.resolve();
  return listen<Call>("studio-operator", ({ payload }) => {
    current = current.then(async () => {
      // A queued event may outlive its original HTTP caller or the preceding
      // host interaction. Claim the still-live request once before executing.
      try {
        await invoke("finish_studio_operator", { id: payload.id, claim: true });
      } catch {
        return;
      }
      let result: unknown = null,
        error: string | null = null;
      try {
        result = await handleStudioAction(payload, host);
      } catch (e) {
        error = String(e);
      }
      await invoke("finish_studio_operator", { id: payload.id, result, error }).catch(() => {});
    });
  });
}

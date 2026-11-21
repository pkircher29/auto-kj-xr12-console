// All controls use scoped operator requests. Host names and catalog strings enter the DOM as text.
function element(tag, text) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  return node;
}

// Desktop webviews can override window.confirm with an asynchronous native API.
export function confirmDeskAction(message, actionLabel) {
  if (document.querySelector("dialog.studio-confirm")) return Promise.resolve(false);
  const dialog = element("dialog");
  if (typeof dialog.showModal !== "function") return Promise.resolve(false);
  dialog.className = "studio-confirm";
  dialog.setAttribute("aria-label", actionLabel);
  const text = element("p", message);
  text.id = "studio-confirm-message";
  dialog.setAttribute("aria-describedby", text.id);
  const cancel = element("button", "Cancel");
  const apply = element("button", actionLabel);
  cancel.autofocus = true;
  dialog.append(text, cancel, apply);
  document.body.append(dialog);
  return new Promise((resolve) => {
    function finish(accepted) {
      document.removeEventListener("visibilitychange", visibilityChanged);
      window.removeEventListener("pagehide", dismiss);
      if (dialog.open) dialog.close();
      dialog.remove();
      resolve(accepted);
    }
    function dismiss() { finish(false); }
    function visibilityChanged() { if (document.hidden) dismiss(); }
    cancel.onclick = dismiss;
    apply.onclick = () => finish(true);
    dialog.addEventListener("cancel", (event) => { event.preventDefault(); dismiss(); });
    document.addEventListener("visibilitychange", visibilityChanged);
    window.addEventListener("pagehide", dismiss);
    try { dialog.showModal(); } catch { dismiss(); }
  });
}

export function renderOperator(parent, request, ask = confirmDeskAction) {
  const box = element("section"),
    nav = element("nav"),
    panel = element("div"),
    status = element("output");
  nav.className = "operator-tabs";
  nav.setAttribute("aria-label", "Show controls");
  panel.className = "operator-controls";
  status.setAttribute("role", "status");
  box.append(nav, panel, status);
  parent.append(box);
  let tab = "",
    ticket,
    overview,
    singer = "",
    partners = [],
    search = "",
    page = 1,
    replacement = "",
    breakPage = 1,
    busy = false;
  let lastRead, displayedView, renewAt, displayedBreak, lastBreakUpdate;

  // Keep this field mounted while the host refreshes the rest of the rotation.
  const addForm = element("div");
  addForm.className = "panel";
  const newSinger = entry(addForm, "New singer");
  const addButton = key(addForm, "Add", submitSinger, true);
  let draftRevision = 0;
  newSinger.oninput = () => {
    draftRevision++;
    updateAddButton();
  };

  function updateAddButton() {
    addButton.disabled = busy || !ticket || !newSinger.value.trim();
  }
  async function submitSinger() {
    const name = newSinger.value.trim();
    if (!name || busy || !ticket) return;
    const submittedRevision = draftRevision;
    newSinger.focus();
    newSinger.select();
    status.textContent = "Adding " + name + "…";
    if (await mutate({ type: "add_singer", name })) {
      if (draftRevision === submittedRevision) newSinger.value = "";
    }
    updateAddButton();
  }
  function key(parent, title, handler, disabled = false) {
    const b = element("button", title);
    b.disabled = disabled;
    b.onclick = () => {
      if (!busy) void handler();
    };
    parent.append(b);
    return b;
  }
  function line(parent, name) {
    const row = element("div");
    row.className = "panel";
    if (name) row.append(element("span", name));
    parent.append(row);
    return row;
  }
  function entry(parent, title, value = "") {
    const label = element("label", title),
      input = element("input");
    input.value = value;
    label.append(input);
    parent.append(label);
    return input;
  }
  function choices(parent, label, rows, value, change) {
    const field = element("label", label),
      select = element("select");
    for (const [id, name] of rows) {
      const option = element("option", name);
      option.value = id;
      select.append(option);
    }
    select.value = value;
    select.onchange = () => {
      if (!busy) void change(select.value);
    };
    field.append(select);
    parent.append(field);
    return select;
  }
  async function read(action) {
    const started = Date.now();
    const data = await request({ action });
    ticket = data.ticket;
    renewAt = started + data.valid_for_ms - 1000;
    overview = data.overview;
    lastRead = action;
    displayedView = JSON.stringify([data.overview, data.view]);
    return data.view;
  }
  async function matchingTicket() {
    const fresh = await request({ action: lastRead });
    if (displayedView !== JSON.stringify([fresh.overview, fresh.view]))
      throw Error("The host changed this view. Check the target before trying again.");
    return fresh.ticket;
  }
  async function confirmChange(action, message, label) {
    const observed = ticket;
    if (!observed || !(await ask(message, label)) || !box.isConnected) return;
    if (ticket !== observed) {
      status.textContent = "The view changed. Check the target and try again.";
      return;
    }
    return mutate(action);
  }
  async function mutate(action) {
    if (!ticket || busy) return;
    busy = true;
    updateAddButton();
    const oneUse = ticket;
    ticket = undefined;
    let message, accepted = false;
    try {
      const current = Date.now() >= renewAt ? await matchingTicket() : oneUse;
      const response = await request({ action, ticket: current });
      accepted = true;
      message = action.type === "add_singer" ? "Added " + action.name :
        typeof response.result === "string" ? response.result : "Host confirmed the change";
    } catch (error) {
      message = error.message;
      if (
        (action.type === "assign" || action.type === "replace") &&
        !action.override_duplicate &&
        message.includes("duplicate policy") &&
        (await ask(message + "\n\nOverride this duplicate for this song?", "Override duplicate")) && box.isConnected
      ) {
        try {
          await request({ ticket: await matchingTicket(), action: { ...action, override_duplicate: true } });
          message = "Host accepted the duplicate override";
        } catch (retryError) {
          message = retryError.message;
        }
      }
    } finally {
      busy = false;
    }
    if (!box.isConnected) return;
    await load();
    status.textContent = message;
    return accepted;
  }
  function redrawNav() {
    nav.replaceChildren();
    parent.classList.toggle("operator-detail", Boolean(tab));
    if (tab) key(nav, "Now playing", () => openTab(""));
    for (const name of ["Rotation", "Requests", "Library", "Break music", "Soundboard", "Show settings"]) {
      const b = key(nav, name, () => openTab(tab === name ? "" : name));
      b.setAttribute("aria-pressed", tab === name);
    }
    if (tab) key(nav, "Refresh", load);
  }
  function openTab(name) {
    tab = name;
    replacement = "";
    partners = [];
    parent.scrollTop = 0;
    return load();
  }
  function picker() {
    if (!overview.singers.some((row) => row.id === singer)) singer = overview.singers[0]?.id || "";
    choices(
      panel,
      "Singer",
      overview.singers.map((row) => [row.id, row.name]),
      singer,
      (value) => {
        singer = value;
        replacement = "";
        partners = [];
        return load();
      },
    );
  }
  function groupPicker() {
    const others = overview.singers.filter(row => row.id !== singer);
    if (!others.length) return;
    const details = element("details"), summary = element("summary"), buttons = new Map();
    details.className = "group-singers";
    details.append(summary);
    panel.append(details);
    function update() {
      const names = partners.map(id => buttons.get(id).textContent);
      summary.textContent = names.length ? "Singing with " + names.join(" + ") : "Add singers for a duet or group";
      for (const [id, button] of buttons) {
        const selected = partners.includes(id);
        button.setAttribute("aria-pressed", selected);
        button.disabled = !selected && partners.length >= 3;
      }
    }
    for (const row of others) {
      buttons.set(row.id, key(details, row.name, () => {
        partners = partners.includes(row.id) ? partners.filter(id => id !== row.id) : [...partners, row.id];
        update();
      }));
    }
    update();
  }
  async function load() {
    if (busy) return;
    busy = true;
    updateAddButton();
    if (tab === "Rotation") {
      for (const child of Array.from(panel.children)) {
        if (child !== addForm) child.remove();
      }
    } else {
      panel.replaceChildren();
      newSinger.value = "";
    }
    status.textContent = "";
    redrawNav();
    try {
      if (!tab) return;
      overview = await read({ type: "overview" });
      if (!box.isConnected) return;
      if (overview.blocked) status.textContent = overview.blocked;
      if (tab === "Rotation") {
        if (addForm.parentNode !== panel) panel.append(addForm);
        if (!overview.singers.length) return;
        if (!overview.singers.some((row) => row.id === singer)) singer = overview.singers[0].id;
        const queue = await read({ type: "queue", singer });
        if (!box.isConnected) return;
        if (!overview.singers.some((row) => row.id === singer))
          throw Error("That singer left the line. Refresh the rotation.");
        picker();
        const selected = overview.singers.find((row) => row.id === singer);
        const row = line(panel);
        const name = entry(row, "Stage name", selected.name);
        key(row, "Rename", () => mutate({ type: "rename_singer", singer, name: name.value.trim() }));
        const slot = overview.slots.findIndex((s) => s.singers.includes(singer));
        const move = (to) =>
          mutate({
            type: "move_singer",
            singer,
            slot: to,
            from: slot,
            target: overview.slots[to].singers[0],
          });
        key(row, "Move up", () => move(slot - 1), slot <= 0);
        key(row, "Move down", () => move(slot + 1), slot < 0 || slot === overview.slots.length - 1);
        key(
          row,
          selected.paused ? "Return to line" : "Pause singer",
          () => mutate({ type: "set_paused", singer, paused: !selected.paused }),
          selected.idle,
        );
        key(row, "Remove singer", () => {
          return confirmChange({ type: "remove_singer", singer }, "Remove " + selected.name + " and their queued songs?", "Remove singer");
        });
        if (!queue.songs.length) panel.append(element("p", "No queued songs"));
        queue.songs.forEach((song, i) => {
          const row = line(panel, song.title + " · " + song.artist + " · " + song.status);
          if (song.is_group) {
            const waiting = song.group_waiting_on || [];
            let label = "Group song";
            if (song.status !== "Playing" && song.group_ready === true) label += " · Ready to sing";
            if (song.group_ready === false) label += " · " + (waiting.join(" · ") || "Waiting for the group");
            row.append(element("span", label));
          }
          if (song.status !== "Queued" && song.status !== "Waiting") return;
          for (const [label, j] of [
            ["Earlier", i - 1],
            ["Later", i + 1],
          ]) {
            const adjacent = queue.songs[j];
            key(
              row,
              label,
              () => mutate({ type: "swap", singer, first: song.id, second: adjacent.id }),
              !adjacent || adjacent.status !== "Queued",
            );
          }
          if (!song.is_group) {
            key(row, "Replace", () => {
              replacement = song.id;
              tab = "Library";
              return load();
            });
          }
          key(row, song.is_group ? "Remove group song" : "Remove song", () => {
            let prompt = "Remove " + song.title + "?";
            if (song.is_group) prompt = "Remove " + song.title + " from every group member’s queue?";
            if (song.is_group == null) prompt += " Shared songs will be removed from every member’s queue.";
            const action = { type: "remove_song", request: song.id };
            if (song.is_group) action.confirm_group = true;
            return confirmChange(action, prompt, song.is_group ? "Remove group song" : "Remove song");
          });
        });
      } else if (tab === "Requests") {
        panel.append(element("p", overview.accepting ? "Requests open" : "Requests closed"));
        key(panel, overview.accepting ? "Close requests" : "Open requests", () =>
          mutate({ type: "accept_requests", enabled: !overview.accepting }),
        );
        if (!overview.inbox.length) panel.append(element("p", "Inbox is empty"));
        for (const card of overview.inbox) {
          const row = line(panel, card.singer + " · " + card.title + " · " + card.artist);
          key(row, "Approve", () => mutate({ type: "review", key: card.key, approve: true }));
          key(row, "Decline", () => mutate({ type: "review", key: card.key, approve: false }));
        }
      } else if (tab === "Library") {
        const results = await read({ type: "search", query: search, page });
        if (!box.isConnected) return;
        if (replacement && !overview.singers.some((row) => row.id === singer)) {
          replacement = "";
          throw Error("The singer selected for replacement left the rotation.");
        }
        if (partners.length && [singer, ...partners].some(id => !overview.singers.some(row => row.id === id))) {
          partners = [];
          throw Error("A selected singer left the rotation. Refresh the Library and select the group again.");
        }
        picker();
        if (!replacement) groupPicker();
        const input = entry(panel, "Search catalog", search);
        function lookup() {
          search = input.value.trim();
          page = 1;
          return load();
        }
        input.onkeydown = (e) => {
          if (!busy && e.key === "Enter") void lookup();
        };
        key(panel, "Search", lookup);
        if (!results.songs.length) panel.append(element("p", "No songs found"));
        for (const song of results.songs) {
          const row = line(panel, song.title + " · " + song.artist + " · " + song.manufacturer);
          key(
            row,
            replacement ? "Use replacement" : "Add to queue",
            () => {
              const action = replacement
                ? { type: "replace", singer, request: replacement, song: song.id }
                : { type: "assign", singer, ...(partners.length ? { partners: [...partners] } : {}), song: song.id };
              replacement = "";
              return mutate(action);
            },
            !singer,
          );
        }
        key(
          panel,
          "Previous results",
          () => {
            page--;
            return load();
          },
          page === 1,
        );
        key(
          panel,
          "Next results",
          () => {
            page++;
            return load();
          },
          !results.more,
        );
      } else if (tab === "Break music") {
        const queue = await read({type:"break_view", page:breakPage});
        if (!box.isConnected) return;
        const live = queue.status;
        displayedBreak = live.revision;
        const sendBreak = (command, track = null) => mutate({type:"break_control",command,track,revision:live.revision});
        panel.append(element("p",live.playing ? "Playing · " + live.title : "Break music stopped"));
        if (live.automix) panel.append(element("p","Automix · " + live.phase));
        if (live.next_title) panel.append(element("p","Up next · " + live.next_title));
        const controls = line(panel);
        key(controls,"Play break",()=>sendBreak("play"),live.playing);
        key(controls,"Stop break",()=>sendBreak("stop"),!live.playing && !live.automix);
        const skipLabel = live.skip_allowed ? "Skip break" : "Skip break (Full)";
        key(controls,skipLabel,()=>sendBreak("skip"),!live.skip_allowed || !live.playing || !live.next);
        if (!queue.tracks.length) panel.append(element("p","The Break-Wave playlist is empty."));
        for (const track of queue.tracks) {
          const prefix=track.id===live.current?"Now · ":track.id===live.next?"Next · ":"";
          const row=line(panel,prefix+track.title+(track.artist?" · "+track.artist:""));
          key(row,"Play track",()=>sendBreak("play",track.id),!!live.automix || (live.playing && track.id===live.current));
        }
        key(panel,"Previous tracks",()=>{breakPage--;return load();},breakPage===1);
        key(panel,"More tracks",()=>{breakPage++;return load();},!queue.more);
      } else if (tab === "Soundboard") {
        for (const pad of overview.pads) key(panel, pad.label, () => mutate({ type: "pad", pad: pad.id }));
      } else {
        const row = line(panel, "Track key · " + overview.pitch + " semitones");
        for (const [label, delta] of [
          ["−1", -1],
          ["+1", 1],
        ]) {
          const target = Math.round(overview.pitch) + delta;
          key(
            row,
            label,
            () => mutate({ type: "key", semitones: target, track: overview.track }),
            overview.playback === "stopped" || target < -6 || target > 6,
          );
        }
        key(
          panel,
          "Start new show",
          () => confirmChange(
            { type: "new_show", confirmed: true },
            "Start a new show and clear tonight’s singers and their queues? The host keeps settings and song history.",
            "Start new show",
          ),
          overview.playback !== "stopped",
        );
        key(panel, "End performances", () => confirmChange(
          { type: "end_performances", confirmed: true },
          "Close song requests, complete the current turn and stop karaoke, break music and soundboard playback? The remaining rotation and mixer stay available.",
          "End performances",
        ));
      }
    } catch (error) {
      status.textContent = error.message;
      ticket = undefined;
    } finally {
      busy = false;
      updateAddButton();
    }
  }
  redrawNav();
  return (revision) => {
    if (busy || !revision || revision === lastBreakUpdate) return;
    lastBreakUpdate = revision;
    if (tab === "Break music" && revision !== displayedBreak) void load();
  };
}

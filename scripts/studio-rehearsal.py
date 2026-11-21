"""Exercise the actual Rust console listeners and packaged desk with a loopback XR12.

Build with cargo build -p local-server --example studio_rehearsal. Supply the
executable as the first argument. Requires Python Playwright and Chromium.
Emulator probes use Android CLI (android layout) when available and otherwise use adb.
Run with --mixer-only to connect a packaged desktop host to the simulated desk.
The standalone desk requires only Python and binds to 127.0.0.1.
"""
import argparse
import json
import shutil
import tempfile
import uuid
import socket
import struct
import subprocess
import threading
import time
from contextlib import closing
from pathlib import Path
from urllib.parse import urlsplit, parse_qs


def osc_string(value):
    data = value.encode() + b"\0"
    return data + b"\0" * (-len(data) % 4)


def unpack_string(data, start=0):
    end = data.index(0, start)
    return data[start:end].decode(), (end + 4) & ~3


def message(path, values):
    tags = []
    content = bytearray()
    for value in values:
        if isinstance(value, str):
            tags.append("s")
            content.extend(osc_string(value))
        elif isinstance(value, int):
            tags.append("i")
            content.extend(struct.pack(">i", value))
        else:
            tags.append("f")
            content.extend(struct.pack(">f", value))
    return osc_string(path) + osc_string("," + "".join(tags)) + content


class Mixer:
    def __init__(self):
        self.udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.udp.bind(("127.0.0.1", 10024))
        self.udp.settimeout(.15)
        self.values = {}
        self.writes = []
        self.last_client = None
        self.meter_subscribers = {}
        self.next_meter_frame = 0.0
        self.stop = threading.Event()
        self.disconnected = threading.Event()
        self.held_query = None
        self.query_received = threading.Event()

    def default(self, path):
        if path in {"/fx/2/type","/fx/3/type","/fx/4/type","/fx/4/insert"}:
            return {"/fx/2/type":11,"/fx/3/type":20,"/fx/4/type":30,"/fx/4/insert":1}[path]
        if path.startswith("/ch/") and path.endswith("/config/insrc"):
            return int(path.split("/")[2]) - 1
        if path.startswith("/headamp/"):
            return { "gain": .5, "phantom": 0 }.get(path.split("/")[-1])
        if path.startswith("/routing/aux/"):
            socket_number = int(path.split("/")[3])
            return {"src": 25 + socket_number, "pos": 10}.get(path.split("/")[-1])
        if path.endswith("/config/name"):
            return "Bench vocal" if path.startswith("/ch/01/") else ""
        if path.endswith(("/mix/fader", "/level")):
            return .6 if path.endswith("/mix/fader") else .4
        if path.endswith("/mix/pan"):
            return .5
        if path.endswith(("/mix/lr", "/mix/on", "/eq/on", "/dyn/on")):
            return 1
        if path.endswith(("/tap", "/grpon", "/insert", "/insert/on", "/type", "/fxslot", "/ratio", "/mode", "/source")):
            return 0
        if "/eq/" in path or "/dyn/" in path or "/par/" in path:
            return .5
        return None

    def serve(self):
        while not self.stop.is_set():
            now = time.monotonic()
            if not self.disconnected.is_set() and now >= self.next_meter_frame:
                self.next_meter_frame = now + .08
                for subscriber, expires in list(self.meter_subscribers.items()):
                    peer, bank = subscriber
                    if now >= expires:
                        del self.meter_subscribers[subscriber]
                        continue
                    if bank == "/meters/1":
                        levels = [-42] * 40
                        levels[1], levels[36], levels[37] = -9, -12, -18
                    else:
                        levels = [0] * 39
                        levels[16], levels[17], levels[32], levels[33], levels[38] = -3, -5, -7, -9, -11
                    blob = struct.pack(f"<I{len(levels)}h", len(levels), *(n * 256 for n in levels))
                    self.udp.sendto(osc_string(bank) + osc_string(",b")
                                    + struct.pack(">I", len(blob)) + blob + bytes(-len(blob) % 4), peer)
            try:
                data, peer = self.udp.recvfrom(8192)
            # Windows reports ICMP from a retired subscription port on recvfrom.
            except (socket.timeout, ConnectionResetError):
                continue
            if self.disconnected.is_set():
                continue
            self.last_client = peer
            path, offset = unpack_string(data)
            tags, offset = unpack_string(data, offset)
            if path == "/xinfo":
                self.udp.sendto(message(path, ["127.0.0.1", "Rehearsal mixer", "XR12", "fixture"]), peer)
                continue
            if path in ("/meters", "/xremote"):
                if path == "/meters" and tags == ",s":
                    bank = unpack_string(data, offset)[0]
                    if bank in ("/meters/1", "/meters/6"):
                        self.meter_subscribers[(peer,bank)] = time.monotonic() + 10
                continue
            if tags in (",f", ",i"):
                value, = struct.unpack_from(">"+tags[-1], data, offset)
                self.values[path] = value
                self.writes.append((path, value))
            elif path == self.held_query:
                self.query_received.set()
                continue
            value = self.values.get(path, self.default(path))
            if value is not None:
                self.udp.sendto(message(path, [value]), peer)

    def external(self, path, value):
        self.values[path] = value
        self.udp.sendto(message(path, [value]), self.last_client)


def browser_rehearsal(url, peer_url, engine, mixer):
    from playwright.sync_api import sync_playwright, expect

    with sync_playwright() as pw:
        browser = pw.chromium.launch()
        errors = []
        for width, height, link in [(1280,800,url), (1024,600,peer_url)]:
            page = browser.new_page(viewport={"width":width,"height":height}, has_touch=width==1024)
            page.on("pageerror", lambda err: errors.append(str(err)))
            page.goto(link)
            expect(page.locator(".strip")).to_have_count(16)
            if width == 1024:
                undersized = page.locator("button,select,.fader input").evaluate_all("""nodes =>
                    nodes.filter(n => n.checkVisibility()).filter(n => {
                        const {width,height} = n.getBoundingClientRect();
                        return width < 48 || height < 48;
                    }).map(n => n.getAttribute('aria-label') || n.textContent)""")
                assert not undersized, f"Tablet controls below 48 dp: {undersized}"
            expect(page.locator("#screen h2")).to_have_text("Bench vocal")
            expect(page.locator("#status")).to_contain_text("Rehearsal mixer connected")
            expect(page.locator("#left")).to_contain_text("-12.0 dBFS")
            expect(page.locator("#right")).to_contain_text("-18.0 dBFS")
            home = page.locator(".strip input[type=range]").nth(1)
            expect(home).to_be_disabled()
            page.get_by_role("button",name="Enable mixer control",exact=True).click()
            expect(home).to_be_enabled()
            if width == 1280:
                confirmed_fader(page, mixer)
            if width == 1024:
                tablet_touch_targets(page, mixer)
                simultaneous_faders(page, mixer)
            page.locator("#pages").get_by_role("button",name="Routing",exact=True).click()
            increase = page.get_by_role("button", name="+0.5 dB", exact=True)
            expect(increase).to_be_enabled()
            if width == 1280:
                raise_level = page.get_by_role("button", name="Raise Main LR fader one step")
                expect(raise_level).to_be_enabled()
                before = len(mixer.writes)
                raise_level.click()
                expect(page.locator("#notice")).to_have_text("Command accepted")
                moved = mixer.writes[before:]
                assert len(moved)==1 and moved[0][0]=="/ch/01/mix/fader"
                assert abs(moved[0][1] - (round(.6*1023)+1)/1023)<.000001
            gain_path = "/headamp/01/gain"
            gain_before = mixer.values.get(gain_path, .5)
            writes_before = len(mixer.writes)
            increase.click()
            expected_gain = (round(gain_before * 144) + 1) / 144
            expect(page.locator("#screen")).to_contain_text(f"{expected_gain * 72 - 12:.1f} dB")
            assert len(mixer.writes) == writes_before + 1
            assert mixer.writes[-1][0] == gain_path and abs(mixer.writes[-1][1] - expected_gain) < .00001
            page.get_by_role("button", name="48 V · off", exact=True).click()
            prompt = page.get_by_role("dialog", name="Enable 48 V")
            expect(prompt).to_contain_text("physical XLR 1")
            expect(prompt.get_by_role("button", name="Cancel")).to_be_focused()
            assert len(mixer.writes) == writes_before + 1
            prompt.get_by_role("button", name="Cancel").click()
            expect(prompt).to_have_count(0)
            assert len(mixer.writes) == writes_before + 1
            if width == 1280:
                delayed_fader(page, mixer)
                interrupted_assignment(page, mixer)
                processing_rehearsal(page, mixer)
            house = mixer.values.get("/ch/02/mix/fader", .6)
            before = len(mixer.writes)
            page.locator("#banks").get_by_role("button",name="Aux 1",exact=True).click()
            page.locator("#pages").get_by_role("button",name="EQ",exact=True).click()
            assert len(mixer.writes)==before, "view switch wrote to mixer"
            home.fill("0.25")
            deadline=time.monotonic()+5
            while mixer.values.get("/ch/02/mix/01/level")!=.25 and time.monotonic()<deadline:
                page.wait_for_timeout(40)
            assert mixer.values["/ch/02/mix/01/level"]==.25
            assert mixer.values.get("/ch/02/mix/fader",.6)==house
            mixer.external("/ch/02/mix/01/level", .625)
            expect(home).to_have_value("0.625")
            page.locator("#pages").get_by_role("button",name="Auto-KJ",exact=True).click()
            page.get_by_role("button",name="Pause",exact=True).click()
            expect(page.get_by_role("button",name="Resume",exact=True)).to_be_enabled()
            engine.stdin.write("resume\n");engine.stdin.flush()
            expect(page.get_by_role("button",name="Pause",exact=True)).to_be_enabled()
            if width == 1024:
                # Hold a gesture across loss of the physical link. Recover from
                # fresh state even if another operator moved the fader meanwhile.
                home.dispatch_event("pointerdown")
                before = len(mixer.writes)
                mixer.disconnected.set()
                try:
                    expect(page.locator("#status")).to_contain_text("XR12 offline", timeout=8000)
                    expect(home).to_be_disabled()
                    expect(page.locator("#left")).to_contain_text("—")
                    page.get_by_role("button", name="Pause", exact=True).click()
                    expect(page.get_by_role("button", name="Resume", exact=True)).to_be_enabled()
                    engine.stdin.write("resume\n"); engine.stdin.flush()
                    mixer.values["/ch/02/mix/01/level"] = .375
                finally:
                    mixer.disconnected.clear()
                expect(page.locator("#status")).to_contain_text("Rehearsal mixer connected", timeout=8000)
                expect(home).to_be_disabled()
                page.get_by_role("button",name="Enable mixer control",exact=True).click()
                expect(home).to_be_enabled(timeout=10000)
                expect(home).to_have_value("0.375")
                home.dispatch_event("change")
                page.wait_for_timeout(250)
                assert len(mixer.writes) == before, "old gesture issued a write after recovery"
                print("Mixer reconnect restored the external level, accepted host transport offline, and discarded the interrupted gesture.")
            if width == 1024:
                feedback_shortcut(page, mixer)
            page.close()
        rejoin_with_peer(browser, url, peer_url, mixer)
        browser.close()
        if errors: raise RuntimeError(errors)
        print("Desktop and tablet-width rehearsal passed: fader isolation, preamp gain, phantom cancellation, external edit and transport readback.")


def tablet_touch_targets(page, mixer):
    """Check the target Chromium actually delivers a touch to, including marks near an edge."""
    from playwright.sync_api import expect

    def check_targets(controls):
        misses = controls.evaluate_all("""elements => {
            const failures = [];
            for (const el of elements) {
                const box = el.getBoundingClientRect();
                const label = el.getAttribute("aria-label") || el.textContent;
                for (const dx of [4, box.width / 2, box.width - 4]) {
                    for (const dy of [4, box.height / 2, box.height - 4]) {
                        const hit = document.elementFromPoint(box.x + dx, box.y + dy);
                        if (!el.contains(hit)) {
                            failures.push([label, Math.round(box.x + dx), Math.round(box.y + dy),
                                hit?.getAttribute("aria-label") || hit?.textContent]);
                        }
                    }
                }
            }
            return failures;
        }""")
        assert not misses, f"Tablet touch intercepted: {misses}"

    def tap(control):
        box = control.bounding_box()
        assert box
        page.touchscreen.tap(box["x"] + box["width"] / 2, box["y"] + box["height"] / 2)

    def check_panel(controls):
        for control in controls.all():
            if control.is_visible():
                control.evaluate("el => el.scrollIntoView({block:'center',inline:'nearest'})")
                check_targets(control)

    bank = page.locator("#banks")
    persistent = page.locator("#banks button,#assign button,.strip-assign button,.strip .fader input,"
                              ".strip .pick,.sendrow button,.strip .lamp")
    writes = len(mixer.writes)
    for label in ("Aux 1", "Aux 2", "Main LR"):
        control = bank.get_by_role("button", name=label, exact=True)
        check_targets(persistent)
        tap(control)
        expect(control).to_have_attribute("aria-pressed", "true")
    assert len(mixer.writes) == writes, "A bank selection wrote to the simulated mixer"

    # The selected strip's Aux 2 assignment used to sit over the Main LR/Aux 1 bank buttons.
    assignment = page.locator("#assign button").nth(2)
    before = assignment.get_attribute("aria-pressed")
    expect(assignment).to_be_enabled()
    tap(assignment)
    expect(assignment).to_have_attribute("aria-pressed", "false" if before == "true" else "true")
    expect(bank.get_by_role("button", name="Main LR", exact=True)).to_have_attribute("aria-pressed", "true")
    tap(assignment)
    expect(assignment).to_have_attribute("aria-pressed", before)
    writes = len(mixer.writes)

    arm = page.locator("#arm")
    tap(arm)
    expect(arm).to_have_text("Enable mixer control")
    check_targets(persistent)
    tap(arm)
    expect(arm).to_have_text("Disable mixer control")
    assert len(mixer.writes) == writes, "Arming or disarming wrote to the simulated mixer"

    pages = page.locator("#pages")
    tap(pages.get_by_role("button", name="Routing", exact=True))
    check_panel(page.locator("#screen button,#screen input,#screen select"))
    tap(pages.get_by_role("button", name="Auto-KJ", exact=True))
    check_panel(page.locator(".now-playing button"))
    page.locator("#screen").evaluate("el => { el.scrollTop = 0; }")


def feedback_shortcut(page, mixer):
    """On the tablet, exercise read-only navigation with known analyzer reports."""
    from playwright.sync_api import expect

    before = len(mixer.writes)
    page.locator(".strip .pick").first.click()
    page.locator("#pages").get_by_role("button", name="Routing", exact=True).click()
    page.locator("#screen label").filter(has_text="Strip input").locator("select").select_option("12")
    bank = page.locator("#banks button[aria-pressed=true]").inner_text()

    report = {"source":11, "post":False, "bin":40, "competing":False, "window_ms":4800,
              "age_ms":50, "mean_interval_ms":50, "candidates":[{"bin":40, "prominence_db":15}]}
    ranges = []
    def with_reading(route):
        response = route.fetch()
        data = response.json()
        data["mixer"].update(analyzer=report, frequency_map=ranges)
        route.fulfill(response=response, json=data)

    page.route("**/api/sync", with_reading)
    try:
        alert = page.locator(".strip .analyzer-alert")
        expect(alert).to_have_count(1)
        expect(alert).to_have_text("Input 12")
        alert.click()
        expect(page.locator("#screen h2")).to_have_text("EQ · Input 12")
        reading = page.locator(".feedback-reading")
        expect(reading).to_be_focused()
        expect(reading).to_contain_text("Rising narrow peak · bin 41")
        expect(reading).not_to_contain_text("Hz")
        expect(page.locator("#banks button[aria-pressed=true]")).to_have_text(bank)
        expect(page.locator(".axis+small")).to_contain_text("One shared analyzer tap")
        expect(page.locator(".axis+small")).to_contain_text("4.8 s observed")
        ranges.append({"bin":40,"low_hz":939.75,"high_hz":1060.25,"tested_low_hz":1000,"tested_high_hz":1000})
        expect(reading).to_contain_text("bin 41 · 939–1061 Hz")
        pin = page.locator(".axis .pin")
        assert pin.evaluate("""p=>{
            const r=p.getBoundingClientRect(), s=document.querySelector('#screen').getBoundingClientRect();
            return r.height>0 && r.top>=s.top && r.bottom<=s.bottom && p.style.left==='40.5%';
        }"""), "Flagged frequency is clipped by the tablet CRT"
        report.update(bin=None, competing=True, candidates=[{"bin":40,"prominence_db":15},{"bin":45,"prominence_db":17}])
        expect(reading).to_contain_text("Several rising peaks")
        expect(pin).to_have_count(2)
        report["source"] = 32
        ranges.clear()
        expect(alert).to_have_count(0)
        expect(reading).to_contain_text("Monitoring monitor output")
        report = None
        expect(reading).to_contain_text("waiting for fresh analyzer data")
        expect(page.locator(".axis")).to_be_hidden()
        assert len(mixer.writes)==before, "Following an analyzer alert wrote to the mixer"
    finally:
        page.unroute_all(behavior="wait")
    print("Tapped a remapped alert strip and focused its visible EQ bin; verified calibrated, competing and expired readings without a mixer write.")


def rejoin_with_peer(browser, first_url, second_url, mixer):
    """Reconnecting one console must preserve a second operator's live mix."""
    from playwright.sync_api import expect
    first, second = browser.new_page(), browser.new_page()
    try:
        for station, link in [(first, first_url), (second, second_url)]:
            station.goto(link)
            expect(station.locator("#status")).to_contain_text("Rehearsal mixer connected")
            arm = station.locator("#arm")
            expect(arm).to_have_text("Enable mixer control")
            arm.click()
            expect(arm).to_have_text("Disable mixer control")
        before = len(mixer.writes)
        second.reload()
        expect(second.locator("#status")).to_contain_text("Rehearsal mixer connected")
        expect(second.locator("#arm")).to_have_text("Enable mixer control")
        expect(second.locator(".strip input[type=range]").nth(4)).to_be_disabled()
        expect(first.locator("#arm")).to_have_text("Disable mixer control")
        fader = first.locator(".strip input[type=range]").nth(4)
        original = fader.input_value()
        fader.fill("0.375")
        expect(second.locator(".strip input[type=range]").nth(4)).to_have_value("0.375")
        assert mixer.writes[before:] == [("/ch/05/mix/fader", .375)]
        fader.fill(original)
        expect(second.locator(".strip input[type=range]").nth(4)).to_have_value(original)
        assert len(mixer.writes) == before + 2
        print("A reconnected station came back read-only while the other stayed armed and mixed channel 5.")
    finally:
        first.close()
        second.close()


def processing_rehearsal(page, mixer):
    from playwright.sync_api import expect
    pages = page.locator("#pages")
    screen = page.locator("#screen")
    picks = page.locator(".strip .pick")
    def set_range(label, value, key):
        control = screen.get_by_role("slider", name=label, exact=True)
        expect(control).to_be_enabled(timeout=10000)
        before = len(mixer.writes)
        control.fill(str(value))
        for _ in range(100):
            if (key, value) in mixer.writes[before:]: break
            page.wait_for_timeout(25)
        assert mixer.writes[before:] == [(key, value)], (key,mixer.writes[before:])
    pages.get_by_role("button",name="Compressor",exact=True).click()
    for index, gain in [(0,-3),(1,-5),(9,-7),(10,-9),(15,-11)]:
        picks.nth(index).click()
        expect(screen).to_contain_text(f"Gain reduction · {gain}.0 dB")
    for index,value in [(0,.25),(1,.75)]:
        picks.nth(index).click()
        set_range("Threshold",value,f"/ch/{index+1:02}/dyn/thr")
    pages.get_by_role("button",name="EQ",exact=True).click()
    for index,value in [(0,.25),(1,.75)]:
        picks.nth(index).click()
        gain=screen.get_by_role("slider",name="Gain",exact=True).first
        expect(gain).to_be_enabled(timeout=10000)
        gain.fill(str(value))
        key=f"/ch/{index+1:02}/eq/1/g"
        for _ in range(100):
            if mixer.values.get(key)==value:break
            page.wait_for_timeout(25)
        assert mixer.values.get(key)==value
    pages.get_by_role("button",name="Effects",exact=True).click()
    for slot in [1,2,3]:
        screen.get_by_role("button", name=f"FX {slot}",exact=True).click()
        set_range(f"FX {slot} return to Aux 1",.25,f"/rtn/{slot}/mix/01/level")
        set_range(f"FX {slot} return to Aux 2",.75,f"/rtn/{slot}/mix/02/level")
    screen.get_by_role("button", name="FX 4",exact=True).click()
    expect(screen.get_by_role("slider",name="FX 4 return to Aux 1",exact=True)).to_be_disabled()
    expect(screen.get_by_role("button",name="Mix FX sends",exact=True)).to_be_disabled()
    pages.get_by_role("button", name="Limiter", exact=True).click()
    picks.nth(9).click()
    expect(screen).to_contain_text("Slot 4 · LIM · Insert")
    def set_insert(label, value, key):
        before = len(mixer.writes)
        screen.get_by_role("combobox", name=label, exact=True).select_option(str(value))
        screen.get_by_role("button", name="Apply "+label, exact=True).click()
        confirm = screen.get_by_role("button", name="Confirm "+label, exact=True)
        expect(confirm).to_be_enabled()
        assert len(mixer.writes) == before
        confirm.click()
        expect(page.locator("#notice")).to_have_text("Command accepted")
        assert mixer.writes[before:] == [(key, value)], mixer.writes[before:]
    target, switch = "/bus/1/insert/fxslot", "/bus/1/insert/on"
    set_insert("insert target", 7, target)
    set_insert("insert switch", 1, switch)
    expect(screen).to_contain_text("Limiter inserted on Aux 1")
    set_insert("insert switch", 0, switch)
    expect(screen).to_contain_text("Selected strip has no confirmed limiter in circuit")
    set_insert("insert target", 0, target)
    print("Confirmed the FX 4A limiter insert on Aux 1, bypassed it and restored its original routing.")
    pages.get_by_role("button",name="Compressor",exact=True).click()
    picks.nth(11).click()
    expect(screen).to_contain_text("FX returns have no channel compressor")
    picks.first.click()
    print("Mapped input and output gain reduction, two channel EQs and compressors, and independent FX returns to both monitor buses.")


def simultaneous_faders(page, mixer):
    controls = page.locator(".strip input[type=range]")
    first, second = controls.nth(2), controls.nth(3)
    from playwright.sync_api import expect
    expect(first).to_be_enabled()
    expect(second).to_be_enabled()
    boxes = [first.bounding_box(), second.bounding_box()]
    def points(positions):
        return [{"x": box["x"] + box["width"] / 2, "y": box["y"] + box["height"] * position,
                 "id": index + 1} for index, (box, position) in enumerate(zip(boxes, positions))]
    before = len(mixer.writes)
    cdp = page.context.new_cdp_session(page)
    try:
        cdp.send("Input.dispatchTouchEvent", {"type":"touchStart","touchPoints":points([.5,.5])})
        for positions in [(.4,.6),(.3,.7),(.17,.83)]:
            cdp.send("Input.dispatchTouchEvent", {"type":"touchMove","touchPoints":points(positions)})
            page.wait_for_timeout(65)
        cdp.send("Input.dispatchTouchEvent", {"type":"touchEnd","touchPoints":[]})
        page.wait_for_timeout(300)
    finally:
        cdp.detach()
    keys = {"/ch/03/mix/fader", "/ch/04/mix/fader"}
    assert {key for key,_ in mixer.writes[before:]} == keys, mixer.writes[before:]
    assert mixer.values["/ch/03/mix/fader"] > .7 and mixer.values["/ch/04/mix/fader"] < .3
    print("Two simultaneous tablet touches moved their own mixer faders in opposite directions.")


def interrupted_assignment(page, mixer):
    """Rearming the same station must never authorize its older pending edit."""
    key = "/ch/01/mix/01/level"
    before = len(mixer.writes)
    mixer.held_query = key
    mixer.query_received.clear()
    try:
        page.evaluate("""async key => {
            const headers = {Authorization: "Bearer " + sessionStorage.getItem("studio.operator"),
                "Content-Type": "application/json"};
            window.fixtureState = async () => (await fetch("/api/state", {headers})).json();
            const state = await window.fixtureState();
            window.fixturePost = (path, args) => fetch("/api/" + path, {method:"POST", headers,
                body: JSON.stringify({epoch:state.mixer.epoch, ...args})}).then(r => r.status);
            window.fixtureAssignment = window.fixturePost("send", {key, enabled:false});
        }""", key)
        assert mixer.query_received.wait(.6), "adapter did not inspect the send"
        assert page.evaluate("() => fixturePost('arm', {enabled:false})") == 204
        assert page.evaluate("async () => fixturePost('arm', {enabled:true,epoch:(await fixtureState()).mixer.epoch})") == 204
        mixer.external(key, .4)
        assert page.evaluate("() => fixtureAssignment") == 409
        assert len(mixer.writes) == before, "superseded arm grant sent a mixer write"
    finally:
        mixer.held_query = None


def confirmed_fader(page, mixer):
    """An older state poll must not visually undo an acknowledged fader move."""
    from playwright.sync_api import expect
    slider = page.get_by_role("slider", name="Input 8 Main LR level", exact=True)
    key = "/ch/08/mix/fader"
    page.evaluate("""() => {
        const original = window.fetch;
        window.holdFaderSnapshot = {count:0, release:null, restore:()=>{window.fetch=original;}};
        window.fetch = async (...args) => {
            const response = await original(...args);
            if (args[0] === "/api/sync") {
                holdFaderSnapshot.count++;
                await new Promise(resolve => {holdFaderSnapshot.release=resolve;});
            }
            return response;
        };
    }""")
    def wait_for_snapshot(count):
        for _ in range(100):
            if page.evaluate("holdFaderSnapshot.count") == count:
                return
            page.wait_for_timeout(20)
        raise AssertionError("Expected a pending console state response")
    try:
        wait_for_snapshot(1)
        with page.expect_response(lambda response: response.url.endswith("/api/mix")) as ack:
            slider.fill("0.825")
        assert ack.value.ok
        expect(slider.locator("..")).not_to_have_attribute("aria-busy", "true")
        assert abs(float(slider.input_value()) - .825) < .001, "Acknowledging the fader restored the old level"
        page.evaluate("holdFaderSnapshot.release()")
        wait_for_snapshot(2)
        assert abs(float(slider.input_value()) - .825) < .001, "An older sync response moved the cap backwards"
        mixer.external(key, .375)
    finally:
        page.evaluate("holdFaderSnapshot.restore(); holdFaderSnapshot.release?.()")
    expect(slider).to_have_value("0.375")
    print("A confirmed fader stayed in place through an older snapshot and followed a later external move.")


def delayed_fader(page, mixer):
    """An in-flight fader edit belongs to the control session where the drag began."""
    from playwright.sync_api import expect
    arm = page.locator("#arm")
    fader = page.locator(".strip input[type=range]").nth(1)
    expect(fader).to_be_enabled()
    pending = []
    before = len(mixer.writes)
    def delay(route):
        pending.append(route)
    page.route("**/api/mix", delay)
    try:
        fader.fill("0.875")
        expect(fader.locator("..")).to_have_attribute("aria-busy", "true")
        for _ in range(100):
            if pending: break
            page.wait_for_timeout(20)
        assert len(pending) == 1
        current_epoch = page.evaluate("""async () => {
            const headers = {Authorization: "Bearer " + sessionStorage.getItem("studio.operator")};
            return (await (await fetch("/api/state", {headers})).json()).mixer.epoch;
        }""")
        assert pending[0].request.post_data_json["epoch"] == current_epoch
        arm.click()
        expect(arm).to_have_text("Enable mixer control")
        arm.click()
        expect(arm).to_have_text("Disable mixer control")
        with page.expect_response(lambda response: response.url.endswith("/api/mix")) as ack:
            pending.pop().continue_()
        assert ack.value.status == 409
        assert len(mixer.writes) == before
        expect(fader).to_have_value("0.6")
    finally:
        for route in pending:
            route.abort()
        page.unroute("**/api/mix", delay)
    print("A delayed fader move was discarded after the same station disarmed and rearmed.")


def android_probe(serial, pairing, adb, screenshots, mixer):
    def cmd(*args,**kwargs):
        result = subprocess.run([adb,"-s",serial,*args],capture_output=True,timeout=25,**kwargs)
        if result.returncode:
            # Activity launch arguments can contain the private operator QR.
            raise RuntimeError(f"Android {args[0]} command exited with {result.returncode}")
        return result
    def present(link):
        cmd("shell", "am", "start", "-n", "com.autokj.studio/.StudioActivity",
            "-a", "android.intent.action.VIEW", "-d", "'" + link + "'")

    layout_cli = shutil.which("android") if serial.startswith("emulator-") else None
    if layout_cli:
        try:
            usage = subprocess.run([layout_cli, "layout", "--help"], capture_output=True, timeout=10)
            if usage.returncode or b"--no-idle" not in usage.stdout + usage.stderr:
                layout_cli = None
        except (OSError, subprocess.TimeoutExpired):
            layout_cli = None
    device_dump = "/data/local/tmp/autokj-rehearsal-" + uuid.uuid4().hex + ".xml"
    endpoint=urlsplit(pairing["tablet_url"])
    cmd("reverse","tcp:"+str(endpoint.port),"tcp:"+str(endpoint.port))
    try:
        # The app opens a local confirmation form before trusting the pin from the QR.
        deeplink="autokj-studio://pair?address="+endpoint.netloc+"#"+endpoint.fragment
        present(deeplink)
        import xml.etree.ElementTree as ET
        import re
        def nodes():
            if layout_cli:
                # The Android CLI keeps native accessibility connected as WebViews are replaced.
                # Its file output is UTF-8 even when Windows console output uses a legacy code page.
                with tempfile.TemporaryDirectory() as private_dir:
                    output = Path(private_dir, "layout.json")
                    result = subprocess.run([layout_cli,"layout","--device",serial,"--full","--flat",
                                             "--no-idle","--output",str(output)],capture_output=True,timeout=25)
                    if result.returncode:
                        raise RuntimeError("Android CLI could not read the emulator's accessibility tree")
                    windows = json.loads(output.read_text(encoding="utf-8"))
                return [node for window in windows if window["type"]=="APPLICATION"
                        for node in window["content"] if not node.get("hidden") and not node.get("off-screen")]
            for attempt in range(3):
                try:
                    cmd("shell","uiautomator","dump",device_dump)
                    return ET.fromstring(cmd("exec-out","cat",device_dump).stdout).iter("node")
                except (RuntimeError, ET.ParseError):
                    if attempt == 2: raise
                    time.sleep(.3)
        def find(label,exact=False):
            for _ in range(12):
                for node in nodes():
                    labels=[node.get(a,"").lower() for a in ("text","content-desc")]
                    if (label.lower() in labels if exact else any(label.lower() in item for item in labels)):
                        return node
                time.sleep(.25)
            raise AssertionError("Android screen did not display "+label)
        def tap(node):
            x,y,r,b=map(int,re.findall(r"\d+",node.get("bounds")))
            cmd("shell","input","tap",str((x+r)//2),str((y+b)//2))
        tap(find("Pair with show host"))
        find("Rehearsal mixer connected")
        find("Rehearsal track")
        tap(find("Enable mixer control",True))
        tap(find("Pause",True))
        find("paused ·")
        tap(find("Resume",True))
        find("playing ·")
        fader=find("Input 2 Main LR level")
        left,top,right,bottom=map(int,re.findall(r"\d+",fader.get("bounds")))
        x=(left+right)//2
        cmd("shell","input","swipe",str(x),str((top+bottom)//2),str(x),str(top+8),"250")
        deadline=time.monotonic()+4
        while "/ch/02/mix/fader" not in mixer.values and time.monotonic()<deadline:time.sleep(.1)
        assert mixer.values.get("/ch/02/mix/fader",0)>.6, "Android fader never reached the mixer"
        if serial.startswith("emulator-"):
            writes_before = len(mixer.writes)
            cmd("shell","input","keyevent","223")
            time.sleep(2)
            writes_asleep = len(mixer.writes)
            cmd("shell","input","keyevent","224")
            cmd("shell","wm","dismiss-keyguard")
            find("Rehearsal mixer connected")
            find("playing ·")
            find("Enable mixer control", True)
            assert len(mixer.writes) == writes_asleep, f"Waking the tablet wrote to the mixer: {mixer.writes[writes_asleep:]}"
            assert all(k == "/ch/02/mix/fader" for k,_ in mixer.writes[writes_before:writes_asleep]), "Unexpected destination while the last gesture finished"
            present(deeplink)
            tap(find("Pair with show host",True))
            find("Enable mixer control",True)
            history=cmd("shell","dumpsys","activity","activities").stdout.decode()
            entries=re.findall(r"Hist\s+#\d+:.*?com\.autokj\.studio/\.StudioActivity\b",history)
            assert len(entries)==1, "Rescanning a QR left earlier consoles on the activity stack"
            assert len(mixer.writes)==writes_asleep, "Rescanning a QR sent an unexpected mixer write"
            token = parse_qs(endpoint.fragment)["token"][0]
            present(deeplink.replace(token, "00000000-0000-0000-0000-000000000000"))
            tap(find("Pair with show host", True))
            find("The show host rejected this pairing")
            cmd("shell", "input", "keyevent", "KEYCODE_BACK")
            find("Rehearsal mixer connected")
            control = find("Enable mixer control", True)
            assert control.get("class") == "android.widget.Button"
            assert (("CLICKABLE" in control.get("interactions", [])) if layout_cli
                    else (control.get("enabled") == "true" and control.get("clickable") == "true"))
            print("Back from a rejected pairing restored the disarmed Android button at " + control.get("bounds"))
            assert len(mixer.writes) == writes_asleep, "Rejecting a pairing sent a mixer write"
        if screenshots:
            png = cmd("exec-out","screencap","-p").stdout
            if not png.startswith(b"\x89PNG\r\n\x1a\n") or not png.endswith(b"IEND\xaeB\x60\x82"):
                raise RuntimeError("Android returned an incomplete screenshot")
            Path(screenshots).write_bytes(png)
        print("Android APK paired via pinned HTTPS, moved a UDP fixture fader, and confirmed rehearsal transport.")
    finally:
        try:
            if not layout_cli:
                cmd("shell","rm","-f",device_dump)
        finally:
            cmd("reverse","--remove","tcp:"+str(endpoint.port))


def main():
    ap=argparse.ArgumentParser()
    ap.add_argument("executable",nargs="?",help="Rust studio_rehearsal example executable")
    ap.add_argument("--mixer-only",action="store_true",help="Run a local simulated XR12 for the desktop host")
    ap.add_argument("--android");ap.add_argument("--adb",default="adb");ap.add_argument("--android-screenshot")
    args=ap.parse_args()
    if args.mixer_only == bool(args.executable) or (args.mixer_only and (args.android or args.android_screenshot)):
        ap.error("Supply the test executable for automation, or --mixer-only for desktop rehearsal.")
    try:
        mixer=Mixer()
    except OSError as error:
        ap.exit(1, f"Could not open the simulated desk on 127.0.0.1:10024. {error}\n")
    with closing(mixer.udp):
        if args.mixer_only:
            print("Rehearsal mixer is listening. Enter 127.0.0.1 in Auto-KJ's XR12 settings. Press Ctrl+C to stop.",flush=True)
            try:
                mixer.serve()
            except KeyboardInterrupt:
                pass
            return
        runner=threading.Thread(target=mixer.serve,daemon=True);runner.start()
        try:
            with subprocess.Popen([args.executable,"--pairing-pipe"],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True) as engine:
                try:
                    a,b,android=json.loads(engine.stdout.readline())
                    browser_rehearsal(a["desktop_url"],b["desktop_url"],engine,mixer)
                    if args.android: android_probe(args.android,android,args.adb,args.android_screenshot,mixer)
                finally:
                    engine.stdin.close()
                    try: engine.wait(timeout=8)
                    except subprocess.TimeoutExpired: engine.kill()
        finally:
            mixer.stop.set();runner.join(timeout=2)


if __name__=="__main__":
    main()

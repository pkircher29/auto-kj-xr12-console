# Auto-KJ XR12 Console

Run a karaoke show and mix a Behringer X AIR XR12 from the same operator desk. The show computer runs Auto-KJ playback, audience lyrics and the mixer connection. An Android tablet on the venue network provides a second desk. Audio reaches the mixer through the show computer’s configured audio output.

## Console specification

| Area | Operator controls and behavior |
| --- | --- |
| Mixing | Nine assignable input strips follow the selected mix. Select Main LR for the house, Aux 1 or Aux 2 for monitors, or one of four FX sends. Aux masters, FX returns and Main LR keep their own destinations. |
| Channel processing | Inspect routing, EQ, dynamics, gain reduction and effects. Preamp controls follow the actual input patch. Phantom power, effect replacement and insert routing require confirmation. |
| Show operation | Control playback, advance singers, adjust pitch, trigger soundboard cues and run break music. The desktop audio engine owns transport. Closing the tablet leaves the show running. |
| Pairing | Pair the Android app through the host’s operator QR code or private HTTPS URL. The app checks the host certificate fingerprint. Each station has its own revocable credential. |
| Mixer connection | The host talks OSC to the XR12 on UDP port 10024. Mixer controls start disarmed. The desk confirms changes from mixer readback and requires rearming after reconnect. |
| Feedback monitoring | The EQ page displays rising peaks on the shared analyzer. Frequency labels require calibration for the current rig. Identifying a specific microphone and verifying cuts require separate measurements. |

## Build and rehearse

The Rust mixer adapter and Android client build from this repository. Running playback, pairing a tablet or opening the desktop console requires a compatible Auto-KJ show host. The host integration files here show how the console connects to that application.

Run the standalone mixer tests with stable Rust from the repository root.

```sh
cargo test --locked --lib
```

Build the Android debug APK with JDK 17 or newer and Android SDK Platform 35. The APK supports Android 9 and later. On Windows use `gradlew.bat assembleDebug` inside `studio-android`.

```sh
cd studio-android
./gradlew assembleDebug
```

To rehearse without mixer hardware, start the Python simulator from the repository root. On the same computer, enter `127.0.0.1` as the mixer address in Auto-KJ. The console should identify **Rehearsal mixer**.

```sh
python scripts/studio-rehearsal.py --mixer-only
```

## Hardware status

The integrated host has read channel mappings, preamps, aux routing, effects and meters from a physical XR12 running firmware 1.26. The standalone tests exercise mixer writes and reconnect behavior against fixtures. End-to-end venue acceptance still requires a tablet fader move confirmed on a muted physical input, an independent monitor-send check and a rehearsal through the actual sound system.

Auto-KJ source is published under its existing proprietary terms. [License](LICENSE) · [Third-party notices](THIRD-PARTY.md)

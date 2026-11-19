// SPDX-License-Identifier: GPL-2.0-or-later
// "Test Sound" of the Spatial Audio page: a short chime in the left channel,
// then the same in the right one, played on the default output. While spatial
// audio is on, the engine's virtual sink is that output, so each chime comes
// from one virtual speaker; in Off it plays in one ear, then the other.
//
// The sound is computed here (no sound file in the extension) and handed to
// GStreamer, which the settings window may lack: then there is no test sound.

import GLib from 'gi://GLib';

export const RATE = 48000;
// One chime, the pause after it, in seconds.
const CHIME_S = 0.42;
const GAP_S = 0.22;
// A soft bell: a fundamental and two quieter partials, decaying fast.
const PITCH_HZ = 784;
const PARTIALS = [[1, 1], [2.01, 0.38], [3.02, 0.14]];
const DECAY_S = 0.11;
const ATTACK_S = 0.004;
const PEAK = 0.3;

// Interleaved stereo float samples: the chime left, a pause, the chime right.
export function chimeSamples(rate = RATE) {
    const chime = Math.round(CHIME_S * rate);
    const gap = Math.round(GAP_S * rate);
    const frames = 2 * chime + gap;
    const samples = new Float32Array(frames * 2);
    const sum = PARTIALS.reduce((s, [, gain]) => s + gain, 0);
    for (let i = 0; i < chime; i++) {
        const t = i / rate;
        const envelope = Math.min(1, t / ATTACK_S) * Math.exp(-t / DECAY_S) *
            // Fades the tail to silence: no click at the end.
            Math.min(1, (chime - 1 - i) / (0.02 * rate));
        let value = 0;
        for (const [ratio, gain] of PARTIALS)
            value += gain * Math.sin(2 * Math.PI * PITCH_HZ * ratio * t);
        value *= PEAK * envelope / sum;
        samples[2 * i] = value;
        samples[2 * (chime + gap + i) + 1] = value;
    }
    return samples;
}

let Gst = null;

// Whether this computer can play it; loads GStreamer the first time.
export async function available() {
    if (Gst)
        return true;
    try {
        ({default: Gst} = await import('gi://Gst?version=1.0'));
        Gst.init(null);
        return ['appsrc', 'audioconvert', 'audioresample', 'autoaudiosink']
            .every(name => Gst.ElementFactory.find(name) !== null);
    } catch {
        Gst = null;
        return false;
    }
}

export class TestSound {
    constructor() {
        this._pipeline = null;
        this._busId = 0;
        this._resolve = null;
    }

    get playing() {
        return this._pipeline !== null;
    }

    // Resolves when the sound ends (or stop()); rejects if it cannot play.
    play() {
        this.stop();
        const pipeline = Gst.parse_launch(
            `appsrc name=src format=time caps=audio/x-raw,format=F32LE,layout=interleaved,rate=${RATE},channels=2 ` +
            '! audioconvert ! audioresample ! autoaudiosink');
        this._pipeline = pipeline;
        const src = pipeline.get_by_name('src');
        const samples = chimeSamples();
        const buffer = Gst.Buffer.new_wrapped_bytes(new GLib.Bytes(new Uint8Array(samples.buffer)));
        buffer.pts = 0;
        buffer.duration = Math.round(samples.length / 2 / RATE * Gst.SECOND);

        return new Promise((resolve, reject) => {
            this._resolve = resolve;
            const bus = pipeline.get_bus();
            this._busId = bus.add_watch(GLib.PRIORITY_DEFAULT, (_bus, message) => {
                if (message.type === Gst.MessageType.EOS) {
                    this.stop();
                } else if (message.type === Gst.MessageType.ERROR) {
                    const [error] = message.parse_error();
                    this._resolve = null;
                    this.stop();
                    reject(error);
                }
                return GLib.SOURCE_CONTINUE;
            });
            src.emit('push-buffer', buffer);
            src.emit('end-of-stream');
            if (pipeline.set_state(Gst.State.PLAYING) === Gst.StateChangeReturn.FAILURE) {
                this._resolve = null;
                this.stop();
                reject(new Error('No sound output'));
            }
        });
    }

    stop() {
        if (!this._pipeline)
            return;
        const pipeline = this._pipeline;
        this._pipeline = null;
        if (this._busId)
            GLib.source_remove(this._busId);
        this._busId = 0;
        pipeline.set_state(Gst.State.NULL);
        this._resolve?.();
        this._resolve = null;
    }
}

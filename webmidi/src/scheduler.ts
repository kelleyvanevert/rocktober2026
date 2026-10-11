// Lookahead scheduler: a timer wakes up often and schedules every frame that falls
// inside the next `lookahead` ms, using MIDI timestamps for sample-tight timing.
// (setTimeout alone is far too jittery to drive a beat directly.)

export type FrameCallback = (time: number, frameDuration: number) => void;

export class Scheduler {
  bpm = 120;
  framesPerBeat = 4; // 16th notes
  lookahead = 100; // ms scheduled ahead
  interval = 25; // ms between timer wakeups

  private timer: number | undefined;
  private _nextTime = 0;

  constructor(private onFrame: FrameCallback) {}

  get running() {
    return this.timer !== undefined;
  }

  get frameDuration() {
    return 60_000 / this.bpm / this.framesPerBeat;
  }

  /** Time at which the next (not yet scheduled) frame will sound. */
  get nextTime() {
    return this._nextTime;
  }

  start() {
    if (this.running) return;
    this._nextTime = performance.now() + 50;
    this.tick();
    this.timer = window.setInterval(() => this.tick(), this.interval);
  }

  stop() {
    window.clearInterval(this.timer);
    this.timer = undefined;
  }

  private tick() {
    const horizon = performance.now() + this.lookahead;
    while (this._nextTime < horizon) {
      this.onFrame(this._nextTime, this.frameDuration);
      this._nextTime += this.frameDuration;
    }
  }
}

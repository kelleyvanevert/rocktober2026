// Lookahead scheduler: a timer wakes up often and schedules every step that falls
// inside the next `lookahead` ms, using MIDI timestamps for sample-tight timing.
// (setTimeout alone is far too jittery to drive a beat directly.)

export type StepCallback = (step: number, time: number, stepDuration: number) => void;

export class Scheduler {
  bpm = 120;
  stepsPerBeat = 4; // 16th notes
  lookahead = 100; // ms scheduled ahead
  interval = 25; // ms between timer wakeups

  private timer: number | undefined;
  private nextTime = 0;
  private step = 0;

  constructor(private onStep: StepCallback) {}

  get running() {
    return this.timer !== undefined;
  }

  get stepDuration() {
    return 60_000 / this.bpm / this.stepsPerBeat;
  }

  start() {
    if (this.running) return;
    this.step = 0;
    this.nextTime = performance.now() + 50;
    this.tick();
    this.timer = window.setInterval(() => this.tick(), this.interval);
  }

  stop() {
    window.clearInterval(this.timer);
    this.timer = undefined;
  }

  private tick() {
    const horizon = performance.now() + this.lookahead;
    while (this.nextTime < horizon) {
      this.onStep(this.step, this.nextTime, this.stepDuration);
      this.nextTime += this.stepDuration;
      this.step++;
    }
  }
}

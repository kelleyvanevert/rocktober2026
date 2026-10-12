// Full-viewport grid of squares, drawn on a canvas. Only whole cells are shown, and
// the grid is centred in the canvas. Cell (0, 0) is top-left.

import { cellKey, type View } from "./world";

export interface Cell {
  col: number;
  row: number;
}

export class Grid {
  static readonly MIN_SIZE = 8;
  static readonly MAX_SIZE = 240;
  static readonly ZOOM_STEP = 1.25;

  /** Cell pitch in CSS pixels (square + gap). */
  size = 40;
  view: View | null = null;
  selected: Cell | null = null;
  onSelect: (cell: Cell | null) => void = () => {};

  private ctx: CanvasRenderingContext2D;
  private width = 0;
  private height = 0;
  private frame = 0;

  constructor(private canvas: HTMLCanvasElement) {
    this.ctx = canvas.getContext("2d")!;
    new ResizeObserver(() => this.resize()).observe(canvas);
    canvas.addEventListener("pointerdown", (e) => this.select(this.cellAt(e.offsetX, e.offsetY)));
    this.resize();
  }

  /** Number of fully visible columns / rows. */
  get cols() {
    return Math.max(1, Math.floor(this.width / this.size));
  }

  get rows() {
    return Math.max(1, Math.floor(this.height / this.size));
  }

  zoomIn() {
    this.setSize(this.size * Grid.ZOOM_STEP);
  }

  zoomOut() {
    this.setSize(this.size / Grid.ZOOM_STEP);
  }

  setSize(size: number) {
    this.size = Math.min(Grid.MAX_SIZE, Math.max(Grid.MIN_SIZE, size));
    this.invalidate();
  }

  show(view: View) {
    this.view = view;
    this.invalidate();
  }

  select(cell: Cell | null) {
    this.selected = cell;
    this.invalidate();
    this.onSelect(cell);
  }

  /** Move the selection by (dx, dy), staying inside the grid; selects 0,0 if nothing is. */
  moveSelection(dx: number, dy: number) {
    const sel = this.selected;
    if (!sel) return this.select({ col: 0, row: 0 });
    const clamp = (v: number, max: number) => Math.min(max - 1, Math.max(0, v));
    this.select({ col: clamp(sel.col + dx, this.cols), row: clamp(sel.row + dy, this.rows) });
  }

  /** Schedule a redraw on the next animation frame (coalesces multiple calls). */
  invalidate() {
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.draw();
    });
  }

  private get origin() {
    return {
      x: Math.floor((this.width - this.cols * this.size) / 2),
      y: Math.floor((this.height - this.rows * this.size) / 2),
    };
  }

  private cellAt(x: number, y: number): Cell | null {
    const { x: ox, y: oy } = this.origin;
    const col = Math.floor((x - ox) / this.size);
    const row = Math.floor((y - oy) / this.size);
    return col >= 0 && row >= 0 && col < this.cols && row < this.rows ? { col, row } : null;
  }

  private resize() {
    const dpr = window.devicePixelRatio || 1;
    const rect = this.canvas.getBoundingClientRect();
    this.width = rect.width;
    this.height = rect.height;
    this.canvas.width = Math.round(rect.width * dpr);
    this.canvas.height = Math.round(rect.height * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.draw();
  }

  private draw() {
    const { ctx, size, cols, rows } = this;
    const { x: ox, y: oy } = this.origin;
    const gap = Math.max(1, Math.round(size * 0.08));
    const cell = size - gap;
    const radius = Math.min(4, cell * 0.1);
    const cells = this.view?.cells;

    ctx.fillStyle = COLORS.bg;
    ctx.fillRect(0, 0, this.width, this.height);
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.font = `${Math.round(cell * 0.4)}px ui-monospace, Menlo, monospace`;

    for (let row = 0; row < rows; row++) {
      for (let col = 0; col < cols; col++) {
        const x = ox + col * size + gap / 2;
        const y = oy + row * size + gap / 2;
        const c = cells?.get(cellKey(col, row));
        ctx.fillStyle = !c
          ? COLORS.empty
          : c.sounding
            ? COLORS.sounding
            : !c.active
              ? COLORS.inactive
              : c.ids.length > 1
                ? COLORS.many
                : COLORS.one;
        ctx.beginPath();
        ctx.roundRect(x, y, cell, cell, radius);
        ctx.fill();

        if (c && c.ids.length > 1 && cell >= 18) {
          ctx.fillStyle = COLORS.count;
          ctx.fillText(String(c.ids.length), x + cell / 2, y + cell / 2 + 1);
        }
      }
    }

    const sel = this.selected;
    if (sel && sel.col < cols && sel.row < rows) {
      ctx.strokeStyle = COLORS.selected;
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.roundRect(ox + sel.col * size + gap / 2 - 1, oy + sel.row * size + gap / 2 - 1, cell + 2, cell + 2, radius + 1);
      ctx.stroke();
    }
  }
}

const COLORS = {
  bg: "#111",
  empty: "#262626",
  inactive: "#363636",
  one: "#5a5a5a",
  many: "#8a8a8a",
  sounding: "#ff5a1f",
  count: "#111",
  selected: "#fff",
};

// Right-hand sidebar. With a cell selected: a "New automaton" editor plus the list of
// automatons currently on that cell. Otherwise: a short language reference.

import type { Cell } from "./grid";
import type { Automaton } from "./world";

const HELP = `click a cell to place an automaton

step right      move (left/right/up/down)
step down 2     move two cells
note c3         play c3 for 1 frame
note c3 dur 4   ...for 4 frames
note c3 channel 2   on MIDI channel 2 (default 1)
every 4 { … }   on every 4th frame
if hit { … }    another automaton is here
if hit >= 2 { … } else { … }
note c3 + hit * 7
                hit, frame are numbers
spawn { … }     new automaton, same cell
step up; note c3  ";" separates (optional)
-- comment

arrows    move selection
⌫         delete all on cell
⌘↵        create / save
space     play / pause (⌘space in editor)
⌘+ ⌘−     zoom
esc       leave editor / deselect`;

export class Sidebar {
  /** These return an error message to show, or null on success. */
  onCreate: (cell: Cell, code: string) => string | null = () => null;
  onUpdate: (id: number, code: string) => string | null = () => null;
  onDelete: (id: number) => void = () => {};
  lookup: (id: number) => Automaton | undefined = () => undefined;

  private help: HTMLElement;
  private panel: HTMLElement;
  private coords: HTMLElement;
  private code: HTMLTextAreaElement;
  private error: HTMLElement;
  private listTitle: HTMLElement;
  private list: HTMLElement;
  private items = new Map<number, Item>();
  private cell: Cell | null = null;

  constructor(root: HTMLElement, initialCode: string, onCodeChange: (code: string) => void) {
    root.innerHTML = `
      <pre class="help"></pre>
      <div class="cell-panel" hidden>
        <h2>cell <span class="coords"></span></h2>
        <section class="panel">
          <h3>New automaton</h3>
          <textarea spellcheck="false" rows="5"></textarea>
          <p class="error"></p>
          <div class="actions">
            <button class="primary create">Create</button>
            <span class="hint">⌘↵</span>
          </div>
        </section>
        <h3 class="list-title"></h3>
        <ul class="automatons"></ul>
      </div>`;
    const $ = <T extends HTMLElement>(sel: string) => root.querySelector(sel) as T;
    this.help = $("pre.help");
    this.panel = $(".cell-panel");
    this.coords = $(".coords");
    this.code = $("textarea");
    this.error = $(".error");
    this.listTitle = $(".list-title");
    this.list = $(".automatons");

    this.help.textContent = HELP;
    this.code.value = initialCode;
    this.code.oninput = () => {
      this.error.textContent = "";
      onCodeChange(this.code.value);
    };
    bindEditorKeys(this.code, () => this.create(), () => this.code.blur());
    $<HTMLButtonElement>("button.create").onclick = () => this.create();
  }

  /** Selection changed. */
  show(cell: Cell | null) {
    const same = cell && this.cell && cell.col === this.cell.col && cell.row === this.cell.row;
    this.cell = cell;
    this.help.hidden = cell !== null;
    this.panel.hidden = cell === null;
    if (same) return;
    this.error.textContent = "";
    this.items.clear();
    this.list.replaceChildren();
    if (cell) this.coords.textContent = `${cell.col},${cell.row}`;
  }

  /**
   * Sync the list with the automatons now on the selected cell. Existing entries are
   * kept (never re-created or moved), so an edit in progress isn't disturbed, and an
   * entry being edited stays even after its automaton has left the cell.
   */
  refresh(ids: number[]) {
    if (!this.cell) return;
    const here = new Set(ids.filter((id) => this.lookup(id)));

    for (const [id, item] of this.items) {
      if (here.has(id)) item.setStatus("");
      else if (item.editing) item.setStatus(this.lookup(id) ? "moved away" : "removed");
      else {
        item.el.remove();
        this.items.delete(id);
      }
    }

    // Ids only ever increase, so sorting keeps existing entries in their relative order
    // and new ones get inserted without moving anything.
    const order = [...new Set([...this.items.keys(), ...here])].sort((a, b) => a - b);
    order.forEach((id, i) => {
      let item = this.items.get(id);
      if (!item) {
        item = new Item(id, this.lookup(id)!.code, this);
        this.items.set(id, item);
      }
      if (this.list.children[i] !== item.el) this.list.insertBefore(item.el, this.list.children[i] ?? null);
    });

    this.listTitle.textContent = here.size ? `On this cell (${here.size})` : "Empty cell";
  }

  private create() {
    if (!this.cell) return;
    this.error.textContent = this.onCreate(this.cell, this.code.value) ?? "";
  }
}

/** One automaton in the list: its code (click to edit) and a delete button. */
class Item {
  el = document.createElement("li");
  editing = false;
  private status: HTMLElement;
  private view: HTMLElement;
  private editor: HTMLElement;
  private textarea: HTMLTextAreaElement;
  private error: HTMLElement;

  constructor(
    private id: number,
    private code: string,
    private sidebar: Sidebar,
  ) {
    this.el.className = "automaton";
    this.el.innerHTML = `
      <div class="head">
        <span class="id">#${id}</span>
        <span class="status"></span>
        <button class="delete" title="Delete">×</button>
      </div>
      <pre class="code" title="Click to edit"></pre>
      <div class="editor" hidden>
        <textarea spellcheck="false"></textarea>
        <p class="error"></p>
        <div class="actions">
          <button class="primary save">Save</button>
          <button class="cancel">Cancel</button>
          <span class="hint">⌘↵</span>
        </div>
      </div>`;
    const $ = <T extends HTMLElement>(sel: string) => this.el.querySelector(sel) as T;
    this.status = $(".status");
    this.view = $("pre.code");
    this.editor = $(".editor");
    this.textarea = $("textarea");
    this.error = $(".error");

    this.view.textContent = code || "(empty)";
    this.view.onclick = () => this.edit();
    this.textarea.oninput = () => (this.error.textContent = "");
    bindEditorKeys(this.textarea, () => this.save(), () => this.close());
    $<HTMLButtonElement>("button.save").onclick = () => this.save();
    $<HTMLButtonElement>("button.cancel").onclick = () => this.close();
    $<HTMLButtonElement>("button.delete").onclick = () => sidebar.onDelete(id);
  }

  setStatus(text: string) {
    this.status.textContent = text;
  }

  private edit() {
    this.editing = true;
    this.view.hidden = true;
    this.editor.hidden = false;
    this.textarea.value = this.code;
    this.textarea.rows = Math.max(3, this.code.split("\n").length + 1);
    this.textarea.focus();
  }

  private close() {
    this.textarea.blur(); // hand keys back to the grid
    this.editing = false;
    this.view.hidden = false;
    this.editor.hidden = true;
    this.error.textContent = "";
  }

  private save() {
    const error = this.sidebar.onUpdate(this.id, this.textarea.value);
    if (error) {
      this.error.textContent = error;
      return;
    }
    this.code = this.textarea.value;
    this.view.textContent = this.code || "(empty)";
    this.close();
  }
}

/** ⌘↵ submits, Tab indents, Esc escapes (without also deselecting the cell). */
function bindEditorKeys(textarea: HTMLTextAreaElement, submit: () => void, escape: () => void) {
  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submit();
    } else if (e.key === "Tab") {
      e.preventDefault();
      document.execCommand("insertText", false, "  "); // keeps undo history
    } else if (e.key === "Escape") {
      e.stopPropagation();
      escape();
    }
  });
}

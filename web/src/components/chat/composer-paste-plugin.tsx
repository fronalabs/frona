"use client";

import { useEffect } from "react";
import { useAui } from "@assistant-ui/react";
import { useLexicalComposerContext } from "@lexical/react/LexicalComposerContext";
import { mergeRegister } from "@lexical/utils";
import {
  $addUpdateTag,
  $generateNodesFromRawText,
  $getSelection,
  $getRoot,
  $isNodeSelection,
  $isRangeSelection,
  BEFORE_INPUT_COMMAND,
  COMMAND_PRIORITY_HIGH,
  PASTE_COMMAND,
  PASTE_TAG,
  type PasteCommandType,
} from "lexical";

function htmlText(html: string): string {
  const body = new DOMParser().parseFromString(html, "text/html").body;
  let text = "";
  let trailingBlockBreak = false;
  const blockBreak = () => {
    if (text && !text.endsWith("\n")) {
      text += "\n";
      trailingBlockBreak = true;
    }
  };
  const visit = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      if (node.textContent) {
        text += node.textContent;
        trailingBlockBreak = false;
      }
      return;
    }
    if (!(node instanceof Element)) return;
    if (/^(SCRIPT|STYLE|TEMPLATE|NOSCRIPT)$/.test(node.tagName)) return;
    if (node.tagName === "BR") {
      text += "\n";
      trailingBlockBreak = false;
      return;
    }
    if (node.matches("td, th") && node.previousElementSibling?.matches("td, th")) {
      text += "\t";
      trailingBlockBreak = false;
    }
    const block = /^(P|DIV|LI|PRE|BLOCKQUOTE|H[1-6]|TR|SECTION|ARTICLE)$/.test(node.tagName);
    if (block) blockBreak();
    node.childNodes.forEach(visit);
    if (block) blockBreak();
  };
  body.childNodes.forEach(visit);
  return trailingBlockBreak ? text.slice(0, -1) : text;
}

// Text-node normalization can change keys/offsets between the two event phases.
// Compare positions in the whole editor so that normalization is not mistaken
// for a user moving the selection.
function $selectionPosition(): string {
  const selection = $getSelection();
  if ($isRangeSelection(selection)) {
    const toAnchor = selection.clone();
    toAnchor.focus.set($getRoot().getKey(), 0, "element");
    const toFocus = selection.clone();
    toFocus.anchor.set($getRoot().getKey(), 0, "element");
    return JSON.stringify({ anchor: toAnchor.getTextContent().length, focus: toFocus.getTextContent().length });
  }
  return JSON.stringify(selection?.getNodes().map((node) => node.getKey()) ?? null);
}

/** Handles both desktop clipboard events and Android's beforeinput-only pastes. */
export function ComposerPastePlugin({ onAttachmentError }: { onAttachmentError: (message: string) => void }) {
  const [editor] = useLexicalComposerContext();
  const aui = useAui();

  useEffect(() => {
    let paired: { source: string; payload: string; selection: string } | null = null;
    let clearPair: ReturnType<typeof setTimeout> | undefined;

    const paste = (event: PasteCommandType): boolean => {
      if (event.type !== "paste" && event.type !== "beforeinput") return false;
      if (editor.isComposing() || ("isComposing" in event && event.isComposing)) return false;
      const transfer = "clipboardData" in event ? event.clipboardData : "dataTransfer" in event ? event.dataTransfer : null;
      let files = Array.from(transfer?.files ?? []);
      if (!files.length) {
        files = Array.from(transfer?.items ?? []).flatMap((item) => {
          const file = item.kind === "file" ? item.getAsFile() : null;
          return file ? [file] : [];
        });
      }
      const plain = transfer?.getData("text/plain");
      const html = transfer?.getData("text/html");
      const uri = transfer?.getData("text/uri-list");
      const text = (plain || (html ? htmlText(html) : "") ||
        uri?.split(/\r\n?|\n/).filter((line) => !line.startsWith("#")).join("\n") ||
        ("data" in event ? event.data : "") || "").replace(/\r\n?/g, "\n");
      if (!files.length && !text) return false;
      const selection = $getSelection();
      if (!files.length && !$isRangeSelection(selection) && !$isNodeSelection(selection)) return false;

      const payload = JSON.stringify({ text: files.length ? "" : text, files: files.map(({ name, size, type, lastModified }) => ({ name, size, type, lastModified })) });
      event.preventDefault();
      // A browser may report one operation in both event phases. Pair only
      // matching opposite phases in this turn, never repeated paste gestures.
      if (paired?.source !== event.type && paired?.payload === payload &&
          paired.selection === $selectionPosition()) {
        paired = null;
        return true;
      }
      if (files.length) {
        for (const file of files) {
          // Upload failures are surfaced by the existing attachment runtime.
          void aui.composer.addAttachment(file).catch((error) => {
            onAttachmentError(error instanceof Error ? error.message : "Could not add pasted file");
          });
        }
      } else if (selection) {
        $addUpdateTag(PASTE_TAG);
        selection.insertNodes($generateNodesFromRawText(text));
      }
      paired = { source: event.type, payload, selection: $selectionPosition() };
      clearTimeout(clearPair);
      clearPair = setTimeout(() => { paired = null; }, 0);
      return true;
    };

    return mergeRegister(
      editor.registerCommand(PASTE_COMMAND, paste, COMMAND_PRIORITY_HIGH),
      editor.registerCommand(BEFORE_INPUT_COMMAND, (event) => {
        if (event.inputType === "insertFromPaste" || event.inputType === "insertFromPasteAsQuotation") return paste(event);
        paired = null;
        return false;
      }, COMMAND_PRIORITY_HIGH),
      () => clearTimeout(clearPair),
    );
  }, [editor, aui, onAttachmentError]);
  return null;
}

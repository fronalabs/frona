import { expect, it } from "vitest";
import { detectContentType } from "../file-manager-utils";
import { canPreviewFile, languageFromFilename } from "@/components/preview/file-preview-content";

it.each([
  ["report.html", " Text/Plain ; charset=utf-8", "text/plain", true],
  ["notes.md", "application/pdf", "application/pdf", false],
  ["source.rs", "image/png", "image/png", false],
  ["data.bin", "application/json; charset=utf-8", "application/json", true],
  ["settings.bin", "application/toml", "application/toml", true],
  ["notes.yaml", "application/x-yaml", "application/x-yaml", true],
  ["nested/BUILD.LOG", "application/octet-stream", "text/plain", true],
  ["C:\\app\\.ENV.LOCAL", "", "text/plain", true],
  ["nested/.GITIGNORE", "application/octet-stream", "text/plain", true],
  ["nested/Dockerfile", "application/octet-stream", "text/x-dockerfile", true],
  ["nested/GNUmakefile", "", "text/x-makefile", true],
  ["nested/unknown.bin", "", "application/octet-stream", false],
])("classifies %s with MIME %s", (filename, mime, expected, preview) => {
  expect(detectContentType(filename, mime)).toBe(expected);
  expect(canPreviewFile(mime, filename)).toBe(preview);
});

it.each([
  [".env", "text/plain"], [".dockerignore", "text/plain"], [".editorconfig", "text/plain"],
  [".npmrc", "text/plain"], [".yarnrc", "text/plain"], [".gitattributes", "text/plain"], [".gitmodules", "text/plain"],
  ["SETTINGS.CONF", "text/plain"], ["settings.cfg", "text/plain"], ["settings.ini", "text/plain"], ["app.properties", "text/plain"],
  ["Makefile", "text/x-makefile"], ["Containerfile", "text/x-dockerfile"], ["CMakeLists.txt", "text/plain"],
  ["READ.MDX", "text/markdown"], ["CODE.TSX", "text/typescript"], ["source.VUE", "text/plain"],
  ["track.ogg", "audio/ogg"], ["track.oga", "audio/ogg"], ["track.opus", "audio/ogg"],
  ["track.flac", "audio/flac"], ["track.aac", "audio/aac"], ["track.m4a", "audio/mp4"],
  ["video.m4v", "video/mp4"], ["video.ogv", "video/ogg"], ["video.mov", "video/quicktime"],
  ["video.mkv", "video/matroska"], ["video.avi", "video/x-msvideo"],
])("detects normalized basename %s", (filename, expected) => {
  expect(detectContentType(`nested/path/${filename.toUpperCase()}`)).toBe(expected);
});

it.each(["video.MKV", "track.FLAC", "archive.zip", "unknown", "image.png", "report.pdf"])("keeps %s as a download fallback", (filename) => {
  expect(canPreviewFile("application/octet-stream", filename)).toBe(false);
});

it.each([["nested/Makefile", "makefile"], ["nested/Containerfile", "dockerfile"], ["C:\\project\\CODE.TSX", "tsx"], ["nested/source.rs", "rust"]])("keeps source highlighting for %s", (filename, language) => {
  expect(languageFromFilename(filename)).toBe(language);
});

it.each(["file.constructor", "file.__proto__", "file.hasOwnProperty", "folder.rs/unknown"])("does not treat %s as source text", (filename) => {
  expect(detectContentType(filename)).toBe("application/octet-stream");
  expect(canPreviewFile("application/octet-stream", filename)).toBe(false);
  expect(languageFromFilename(filename)).toBeNull();
});

it.each(["JS", "MJS", "TS", "TSX", "RS", "PY", "CSS", "CPP", "VUE", "SVELTE", "ASTRO", "MDX", "MD"])("preserves generic source/Markdown preview for .%s", (extension) => {
  expect(canPreviewFile("application/octet-stream", `nested/source.${extension}`)).toBe(true);
});

it.each([undefined, null, "", "  "])("uses filename fallback for missing MIME %s", (mime) => {
  expect(detectContentType("SETTINGS.CONF", mime)).toBe("text/plain");
});

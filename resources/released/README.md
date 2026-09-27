The plugin files earlier releases wrote, byte for byte, taken from their tags.

`omarchy-dockctl` compares what it finds in a plugin directory against these
(and against its own copies) before it overwrites or deletes anything, so an
upgrade from one of these releases is recognised as the dock's own files while
anything else — edited, or another plugin's — is left alone.

- `1.2.5/`, `1.2.4/`, `1.2.3/`, `1.2.2/`, `1.2.1/` —
  `plugins/io.github.szlukabence.omarchy-dock/`.
  1.2.1 kept no copies of what it wrote; later releases did, but their files
  are here too, in case those copies are gone. Add each release's files here
  after tagging it.
- `1.2.0/` — `plugins/omarchy-dock/`, the last release under the old id.

The `.txt` suffix keeps tools that look for plugin manifests from mistaking
these for a plugin.

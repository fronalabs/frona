---
name: web_fetch
parameters:
  url:
    type: string
    description: URL of the web page to fetch
required:
  - url
---
Fetch a web page using a full browser with JavaScript rendering and return its content as markdown. Prefer this over curl for web pages, as it executes JavaScript and captures dynamically loaded content.

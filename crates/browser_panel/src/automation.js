// AI tab instrumentation for the built-in browser (ADR-F89).
//
// Injected at document start into AI tabs only. It records the page's console
// messages and fetch / XMLHttpRequest traffic, hands out element refs, and
// implements the page side of the browser tools (snapshot, find, synthetic
// input, form values, waiting, evaluation results). The Rust side calls it
// through `window.__forgeKairosAutomation.call(name, args)`.
//
// The page shares this world, so it can tamper with everything here. That is
// acceptable for a debugging tool: the network ceiling is enforced by the
// egress proxy, not by this script, and the editor never trusts values from
// here for security decisions.
//
// Rules this file keeps: never throw out of a wrapper, keep `this`, arguments
// and return values of everything it wraps, define no global other than the
// namespace, and do nothing when injected a second time.
(function () {
  "use strict";

  var NAMESPACE = "__forgeKairosAutomation";
  try {
    if (Object.prototype.hasOwnProperty.call(window, NAMESPACE)) {
      return;
    }
  } catch (error) {
    return;
  }

  var CONSOLE_LIMIT = 500;
  var NETWORK_LIMIT = 300;
  var MESSAGE_LIMIT = 2000;
  var BODY_LIMIT = 16384;
  var LARGEST_READ_BODY = 1048576;
  var EVAL_LIMIT = 100000;
  var NAME_LIMIT = 100;
  var TEXT_NODE_LIMIT = 200;
  var REF_LIMIT = 5000;
  var PENDING_EVALUATION_LIMIT = 100;

  // Built-ins are captured now, before the page has had a chance to replace them.
  var jsonStringify = JSON.stringify;
  var objectToString = Object.prototype.toString;
  var hasWeakRef = typeof WeakRef === "function";
  var nativeConsole = window.console;

  // Lookup tables without a prototype, so that a role or tag named like an
  // Object.prototype member ("constructor") or a polluted prototype reads as absent.
  function table(entries) {
    var result = Object.create(null);
    Object.keys(entries).forEach(function (key) {
      result[key] = entries[key];
    });
    return result;
  }

  function now() {
    try {
      return performance.now();
    } catch (error) {
      return 0;
    }
  }

  function automationError(message) {
    var error = new Error(message);
    error.automation = true;
    return error;
  }

  function cap(text, limit) {
    text = String(text);
    return text.length > limit ? text.slice(0, limit) + "…" : text;
  }

  function errorMessage(error) {
    try {
      if (error && typeof error === "object" && "message" in error) {
        var name = error.name ? String(error.name) : "Error";
        return name + ": " + String(error.message);
      }
      return String(error);
    } catch (inner) {
      return "unprintable error";
    }
  }

  function errorStack(error) {
    try {
      return error && error.stack ? String(error.stack) : "";
    } catch (inner) {
      return "";
    }
  }

  // ---------------------------------------------------------------------------
  // Console
  // ---------------------------------------------------------------------------

  var consoleEntries = [];
  var consoleDropped = 0;

  function pushConsole(level, text, source) {
    consoleEntries.push({
      level: level,
      text: cap(text, MESSAGE_LIMIT),
      time: now(),
      source: source || null,
    });
    if (consoleEntries.length > CONSOLE_LIMIT) {
      consoleEntries.shift();
      consoleDropped += 1;
    }
  }

  function describeForLog(value) {
    try {
      if (typeof value === "string") {
        return value;
      }
      if (value instanceof Error) {
        return value.stack ? String(value.stack) : errorMessage(value);
      }
      if (value === null || value === undefined || typeof value !== "object") {
        return typeof value === "function"
          ? "[Function " + (value.name || "anonymous") + "]"
          : String(value);
      }
      if (value.nodeType === 1) {
        return describeElement(value);
      }
      var text = safeJson(value, MESSAGE_LIMIT).json;
      return text === undefined ? objectToString.call(value) : text;
    } catch (error) {
      return "[unprintable]";
    }
  }

  ["log", "info", "warn", "error", "debug"].forEach(function (level) {
    try {
      var original = nativeConsole && nativeConsole[level];
      if (typeof original !== "function") {
        return;
      }
      var wrapped = function () {
        try {
          var parts = [];
          for (var index = 0; index < arguments.length; index += 1) {
            parts.push(describeForLog(arguments[index]));
          }
          pushConsole(level, parts.join(" "), null);
        } catch (error) {
          // Logging must never break the page's own call.
        }
        return original.apply(this, arguments);
      };
      nativeConsole[level] = wrapped;
    } catch (error) {
      // A frozen console stays uninstrumented.
    }
  });

  try {
    window.addEventListener(
      "error",
      function (event) {
        try {
          var target = event && event.target;
          if (target && target !== window && target.tagName) {
            pushConsole(
              "error",
              "Failed to load " +
                String(target.tagName).toLowerCase() +
                ": " +
                (target.currentSrc || target.src || target.href || ""),
              null
            );
            return;
          }
          var error = event.error;
          var text =
            "Uncaught " +
            (error && error.stack ? String(error.stack) : String(event.message || error));
          var source = event.filename
            ? event.filename + ":" + event.lineno + ":" + event.colno
            : null;
          pushConsole("exception", text, source);
        } catch (inner) {
          // Ignored: see above.
        }
      },
      true
    );
    window.addEventListener("unhandledrejection", function (event) {
      try {
        var reason = event.reason;
        pushConsole(
          "rejection",
          "Unhandled promise rejection: " +
            (reason && reason.stack ? String(reason.stack) : describeForLog(reason)),
          null
        );
      } catch (inner) {
        // Ignored: see above.
      }
    });
  } catch (error) {
    // No listeners on this window.
  }

  // A modal dialog would stop every later script (and so every tool call) until
  // a person dismisses it. In AI tabs the dialogs answer at once instead and
  // leave a console entry, so the model can see what the page asked.
  function answerDialog(kind, message, answer, answerText) {
    pushConsole(
      "dialog",
      kind + "(" + jsonStringify(String(message === undefined ? "" : message)) + ") was answered " +
        answerText + " without showing it",
      null
    );
    return answer;
  }

  try {
    window.alert = function (message) {
      try {
        answerDialog("alert", message, undefined, "automatically");
      } catch (error) {
        // Ignored: the page only expects alert to return.
      }
    };
    window.confirm = function (message) {
      try {
        answerDialog("confirm", message, false, "with false (Cancel)");
      } catch (error) {
        // Ignored: the answer is the same.
      }
      return false;
    };
    window.prompt = function (message) {
      try {
        answerDialog("prompt", message, null, "with null (Cancel)");
      } catch (error) {
        // Ignored: the answer is the same.
      }
      return null;
    };
  } catch (error) {
    // The dialogs stay native.
  }

  // print() stops the page's scripts until someone closes the print preview,
  // and from there the AI could print. In AI tabs it only leaves a note.
  try {
    window.print = function () {
      try {
        pushConsole("dialog", "print() was ignored without showing the print dialog", null);
      } catch (error) {
        // Ignored: the page only expects print to return.
      }
    };
  } catch (error) {
    // print stays native.
  }

  // The backends deny permission requests in AI tabs, but WebKit on macOS has
  // no public way to answer one for geolocation, and its prompt would wait for
  // a person. The page gets the denial it would get from a user who said no.
  try {
    if (window.Geolocation && Geolocation.prototype) {
      var denyPosition = function (failure) {
        if (typeof failure === "function") {
          setTimeout(function () {
            failure({
              code: 1,
              message: "Location is not available in AI tabs.",
              PERMISSION_DENIED: 1,
              POSITION_UNAVAILABLE: 2,
              TIMEOUT: 3,
            });
          }, 0);
        }
      };
      Geolocation.prototype.getCurrentPosition = function (success, failure) {
        denyPosition(failure);
      };
      Geolocation.prototype.watchPosition = function (success, failure) {
        denyPosition(failure);
        return 0;
      };
    }
  } catch (error) {
    // Geolocation stays native; the backend's own denial still applies.
  }

  // ---------------------------------------------------------------------------
  // Network
  // ---------------------------------------------------------------------------

  var networkEntries = [];
  var networkDropped = 0;
  var nextRequest = 1;
  var SENSITIVE_HEADER = /^(authorization|proxy-authorization|cookie|set-cookie|x-api-key|x-auth-token|x-csrf-token|x-xsrf-token)$/i;
  var SENSITIVE_KEY = /pass(word)?|passwd|pwd|secret|token|otp|cvv|cvc|card.?number|cc.?(num|number|csc|exp)/i;

  function absoluteUrl(url) {
    try {
      return new URL(String(url), document.baseURI).href;
    } catch (error) {
      return String(url);
    }
  }

  function redactHeaders(pairs) {
    return pairs.map(function (pair) {
      return SENSITIVE_HEADER.test(pair[0]) ? [pair[0], "[redacted]"] : pair;
    });
  }

  function headerPairs(headers) {
    var pairs = [];
    try {
      if (!headers) {
        return pairs;
      }
      if (typeof headers.forEach === "function" && !Array.isArray(headers)) {
        headers.forEach(function (value, name) {
          pairs.push([String(name), String(value)]);
        });
      } else if (Array.isArray(headers)) {
        headers.forEach(function (pair) {
          pairs.push([String(pair[0]), String(pair[1])]);
        });
      } else if (typeof headers === "object") {
        Object.keys(headers).forEach(function (name) {
          pairs.push([name, String(headers[name])]);
        });
      }
    } catch (error) {
      // Unreadable headers are left out.
    }
    return redactHeaders(pairs);
  }

  // A `"key": value` pair of JSON that may be cut off (the value too).
  var JSON_PAIR = /("(?:[^"\\]|\\.)*")(\s*:\s*)("(?:[^"\\]|\\.)*"?|[^,}\]\s]+)/g;

  // Credentials the user types while signing in inside an AI tab must not
  // reach the model through the request log. Runs on the whole body, before
  // it is cut to size, so a long body cannot hide them.
  function redactBody(text) {
    if (typeof text !== "string") {
      return text;
    }
    try {
      var parsed = JSON.parse(text);
      if (parsed && typeof parsed === "object") {
        var redact = function (value) {
          if (Array.isArray(value)) {
            return value.map(redact);
          }
          if (value && typeof value === "object") {
            var copy = Object.create(null);
            Object.keys(value).forEach(function (key) {
              copy[key] = SENSITIVE_KEY.test(key) ? "[redacted]" : redact(value[key]);
            });
            return copy;
          }
          return value;
        };
        return cap(jsonStringify(redact(parsed)), BODY_LIMIT);
      }
    } catch (error) {
      // Not (complete) JSON; see below.
    }
    var start = text.replace(/^\s+/, "").charAt(0);
    if (start === "{" || start === "[") {
      return cap(
        text.replace(JSON_PAIR, function (pair, key, colon) {
          var name = key.slice(1, -1);
          try {
            name = JSON.parse(key);
          } catch (error) {
            // Keep the raw key.
          }
          return SENSITIVE_KEY.test(name) ? key + colon + '"[redacted]"' : pair;
        }),
        BODY_LIMIT
      );
    }
    if (/^[^=&\s]+=[^&]*(&[^=&\s]+=[^&]*)*$/.test(text)) {
      return cap(
        text
          .split("&")
          .map(function (part) {
            var key = part.split("=")[0];
            var decoded = key;
            try {
              decoded = decodeURIComponent(key);
            } catch (error) {
              // Keep the raw key.
            }
            return SENSITIVE_KEY.test(decoded) ? key + "=[redacted]" : part;
          })
          .join("&"),
        BODY_LIMIT
      );
    }
    return cap(text, BODY_LIMIT);
  }

  function describeBody(body) {
    try {
      if (body === null || body === undefined) {
        return null;
      }
      if (typeof body === "string") {
        return cap(body, LARGEST_READ_BODY);
      }
      if (typeof URLSearchParams === "function" && body instanceof URLSearchParams) {
        return cap(body.toString(), LARGEST_READ_BODY);
      }
      if (typeof FormData === "function" && body instanceof FormData) {
        var parts = [];
        body.forEach(function (value, key) {
          parts.push(
            encodeURIComponent(key) +
              "=" +
              (typeof value === "string"
                ? encodeURIComponent(value)
                : "[file " + (value && value.name ? value.name : "") + "]")
          );
        });
        return cap(parts.join("&"), LARGEST_READ_BODY);
      }
      if (typeof Blob === "function" && body instanceof Blob) {
        return "[Blob, " + body.size + " bytes]";
      }
      if (
        typeof ArrayBuffer === "function" &&
        (body instanceof ArrayBuffer || ArrayBuffer.isView(body))
      ) {
        return "[binary, " + body.byteLength + " bytes]";
      }
      return "[" + objectToString.call(body) + "]";
    } catch (error) {
      return "[unreadable body]";
    }
  }

  function startRequest(kind, method, url) {
    var entry = {
      id: "r" + nextRequest,
      kind: kind,
      method: String(method || "GET").toUpperCase(),
      url: absoluteUrl(url),
      status: null,
      statusText: "",
      error: null,
      pending: true,
      startTime: now(),
      duration: null,
      contentType: "",
      requestHeaders: [],
      requestBody: null,
      responseHeaders: [],
      responseBody: null,
      responseBodyTruncated: false,
      responseBodyNote: null,
    };
    nextRequest += 1;
    networkEntries.push(entry);
    if (networkEntries.length > NETWORK_LIMIT) {
      networkEntries.shift();
      networkDropped += 1;
    }
    return entry;
  }

  function finishRequest(entry) {
    entry.pending = false;
    entry.duration = now() - entry.startTime;
  }

  function isTextLike(contentType) {
    return /^(text\/|application\/([\w.+-]*\+)?(json|xml|javascript|x-www-form-urlencoded|graphql))/i.test(
      contentType
    );
  }

  function readLimited(stream, limit) {
    var reader = stream.getReader();
    var decoder = new TextDecoder();
    var text = "";
    function step() {
      return reader.read().then(function (chunk) {
        if (chunk.done) {
          text += decoder.decode();
          return { text: text, truncated: false };
        }
        text += decoder.decode(chunk.value, { stream: true });
        if (text.length >= limit) {
          reader.cancel().catch(function () {});
          return { text: text.slice(0, limit), truncated: true };
        }
        return step();
      });
    }
    return step();
  }

  function recordFetchResponse(entry, response) {
    entry.status = response.status;
    entry.statusText = response.statusText || "";
    entry.responseHeaders = headerPairs(response.headers);
    entry.contentType = response.headers.get("content-type") || "";
    finishRequest(entry);
    if (!isTextLike(entry.contentType) || /^text\/event-stream/i.test(entry.contentType)) {
      entry.responseBodyNote = entry.contentType
        ? "not recorded (" + entry.contentType + ")"
        : "not recorded";
      return;
    }
    var length = Number(response.headers.get("content-length"));
    if (length > LARGEST_READ_BODY) {
      entry.responseBodyNote = "not recorded (" + length + " bytes)";
      return;
    }
    if (!response.body) {
      entry.responseBodyNote = "empty";
      return;
    }
    var copy;
    try {
      copy = response.clone();
    } catch (error) {
      entry.responseBodyNote = "not recorded (the body was already read)";
      return;
    }
    readLimited(copy.body, BODY_LIMIT).then(
      function (result) {
        entry.responseBody = result.text;
        entry.responseBodyTruncated = result.truncated;
      },
      function () {
        entry.responseBodyNote = "could not be read";
      }
    );
  }

  try {
    var originalFetch = window.fetch;
    if (typeof originalFetch === "function") {
      window.fetch = function (input, init) {
        var entry = null;
        try {
          var isRequest = typeof Request === "function" && input instanceof Request;
          var method = (init && init.method) || (isRequest ? input.method : "GET");
          var url = isRequest ? input.url : String(input);
          entry = startRequest("fetch", method, url);
          entry.requestHeaders = headerPairs(
            init && init.headers ? new Headers(init.headers) : isRequest ? input.headers : null
          );
          entry.requestBody = redactBody(describeBody(init && init.body));
        } catch (error) {
          entry = null;
        }
        var result = originalFetch.apply(this, arguments);
        if (entry && result && typeof result.then === "function") {
          try {
            result.then(
              function (response) {
                try {
                  recordFetchResponse(entry, response);
                } catch (error) {
                  finishRequest(entry);
                }
              },
              function (error) {
                try {
                  entry.error = errorMessage(error);
                  finishRequest(entry);
                } catch (inner) {
                  // Ignored: the page gets the rejection from `result`.
                }
              }
            );
          } catch (error) {
            // Ignored: see above.
          }
        }
        return result;
      };
    }
  } catch (error) {
    // fetch stays uninstrumented.
  }

  try {
    var xhrPrototype = window.XMLHttpRequest && window.XMLHttpRequest.prototype;
    if (xhrPrototype) {
      var originalOpen = xhrPrototype.open;
      var originalSend = xhrPrototype.send;
      var originalSetRequestHeader = xhrPrototype.setRequestHeader;
      var xhrRequests = new WeakMap();

      xhrPrototype.open = function (method, url) {
        try {
          xhrRequests.set(this, { method: method, url: url, headers: [] });
        } catch (error) {
          // Ignored: the request is just not recorded.
        }
        return originalOpen.apply(this, arguments);
      };

      xhrPrototype.setRequestHeader = function (name, value) {
        try {
          var request = xhrRequests.get(this);
          if (request) {
            request.headers.push([String(name), String(value)]);
          }
        } catch (error) {
          // Ignored: see above.
        }
        return originalSetRequestHeader.apply(this, arguments);
      };

      xhrPrototype.send = function (body) {
        try {
          var request = xhrRequests.get(this);
          if (request) {
            var entry = startRequest("xhr", request.method, request.url);
            entry.requestHeaders = redactHeaders(request.headers);
            entry.requestBody = redactBody(describeBody(body));
            var xhr = this;
            xhr.addEventListener("loadend", function () {
              try {
                recordXhrResponse(entry, xhr);
              } catch (error) {
                finishRequest(entry);
              }
            });
          }
        } catch (error) {
          // Ignored: see above.
        }
        return originalSend.apply(this, arguments);
      };
    }
  } catch (error) {
    // XMLHttpRequest stays uninstrumented.
  }

  function recordXhrResponse(entry, xhr) {
    finishRequest(entry);
    entry.status = xhr.status;
    entry.statusText = xhr.statusText || "";
    if (xhr.status === 0) {
      entry.error = "network error, or the request was aborted";
      entry.status = null;
      return;
    }
    var raw = xhr.getAllResponseHeaders() || "";
    entry.responseHeaders = redactHeaders(
      raw
        .split(/\r?\n/)
        .filter(function (line) {
          return line.indexOf(":") > 0;
        })
        .map(function (line) {
          var colon = line.indexOf(":");
          return [line.slice(0, colon).trim(), line.slice(colon + 1).trim()];
        })
    );
    entry.contentType = xhr.getResponseHeader("content-type") || "";
    if (xhr.responseType === "" || xhr.responseType === "text") {
      var text = xhr.responseText || "";
      entry.responseBody = text.slice(0, BODY_LIMIT);
      entry.responseBodyTruncated = text.length > BODY_LIMIT;
    } else if (xhr.responseType === "json") {
      var json = jsonStringify(xhr.response);
      json = json === undefined ? "" : json;
      entry.responseBody = json.slice(0, BODY_LIMIT);
      entry.responseBodyTruncated = json.length > BODY_LIMIT;
    } else {
      entry.responseBodyNote = "not recorded (responseType " + xhr.responseType + ")";
    }
  }

  // ---------------------------------------------------------------------------
  // Element refs
  // ---------------------------------------------------------------------------

  var refs = new Map();
  var refOfElement = new WeakMap();
  var nextRef = 1;

  function cssEscape(text) {
    if (window.CSS && typeof CSS.escape === "function") {
      return CSS.escape(text);
    }
    return String(text).replace(/[^\w-]/g, "\\$&");
  }

  // A selector that finds the element again if the page replaces the node
  // (common after re-renders). Only for the top document.
  function selectorPath(element) {
    try {
      if (element.ownerDocument !== document || !(element.getRootNode() instanceof Document)) {
        return null;
      }
      var steps = [];
      var current = element;
      while (current && current.nodeType === 1 && current !== document.documentElement) {
        if (current.id && document.querySelectorAll("#" + cssEscape(current.id)).length === 1) {
          steps.unshift("#" + cssEscape(current.id));
          return steps.join(" > ");
        }
        var tag = current.tagName.toLowerCase();
        var index = 1;
        var sibling = current.previousElementSibling;
        while (sibling) {
          if (sibling.tagName === current.tagName) {
            index += 1;
          }
          sibling = sibling.previousElementSibling;
        }
        steps.unshift(tag + ":nth-of-type(" + index + ")");
        current = current.parentElement;
      }
      steps.unshift("html");
      return steps.join(" > ");
    } catch (error) {
      return null;
    }
  }

  function refFor(element) {
    var existing = refOfElement.get(element);
    if (existing && refs.has(existing)) {
      return existing;
    }
    var ref = "e" + nextRef;
    nextRef += 1;
    refs.set(ref, {
      weak: hasWeakRef ? new WeakRef(element) : null,
      strong: hasWeakRef ? null : element,
      path: selectorPath(element),
    });
    refOfElement.set(element, ref);
    if (refs.size > REF_LIMIT) {
      refs.delete(refs.keys().next().value);
    }
    return ref;
  }

  function resolveRef(ref) {
    var record = refs.get(String(ref));
    if (!record) {
      throw automationError(
        "No element has the ref `" +
          ref +
          "`. Refs stop working when the page navigates; take a new browser_snapshot."
      );
    }
    var element = record.weak ? record.weak.deref() : record.strong;
    if (!element || !element.isConnected) {
      element = null;
      if (record.path) {
        try {
          element = document.querySelector(record.path);
        } catch (error) {
          element = null;
        }
      }
      if (!element) {
        throw automationError(
          "The element `" + ref + "` is no longer on the page; take a new browser_snapshot."
        );
      }
      record.weak = hasWeakRef ? new WeakRef(element) : null;
      record.strong = hasWeakRef ? null : element;
      refOfElement.set(element, String(ref));
    }
    return element;
  }

  // ---------------------------------------------------------------------------
  // Roles, names and visibility
  // ---------------------------------------------------------------------------

  var INTERACTIVE_ROLES = table({
    button: true, link: true, checkbox: true, radio: true, switch: true, tab: true,
    menuitem: true, menuitemcheckbox: true, menuitemradio: true, option: true,
    textbox: true, searchbox: true, combobox: true, listbox: true, slider: true,
    spinbutton: true, treeitem: true,
  });
  var NAME_FROM_CONTENT = table({
    button: true, link: true, heading: true, option: true, tab: true, menuitem: true,
    menuitemcheckbox: true, menuitemradio: true, treeitem: true, cell: true,
    columnheader: true, rowheader: true, switch: true, checkbox: true, radio: true,
    tooltip: true, summary: true,
  });
  var TEXT_INPUT_TYPES = table({
    "": true, text: true, search: true, email: true, url: true, tel: true, password: true,
    number: true, date: true, "datetime-local": true, month: true, time: true, week: true,
  });

  function styleOf(element) {
    try {
      var view = element.ownerDocument.defaultView || window;
      return view.getComputedStyle(element);
    } catch (error) {
      return null;
    }
  }

  function tagOf(element) {
    return element && element.tagName ? element.tagName.toLowerCase() : "";
  }

  function inputType(element) {
    return tagOf(element) === "input" ? String(element.type || "text").toLowerCase() : "";
  }

  function roleOf(element) {
    var explicit = (element.getAttribute("role") || "").trim().split(/\s+/)[0];
    if (explicit) {
      return explicit.toLowerCase();
    }
    var tag = tagOf(element);
    switch (tag) {
      case "a":
      case "area":
        return element.hasAttribute("href") ? "link" : null;
      case "button":
        return "button";
      case "input": {
        var type = inputType(element);
        if (type === "hidden") return null;
        if (type === "checkbox") return "checkbox";
        if (type === "radio") return "radio";
        if (type === "range") return "slider";
        if (type === "number") return "spinbutton";
        if (type === "search") return "searchbox";
        if (/^(button|submit|reset|image|file)$/.test(type)) return "button";
        return "textbox";
      }
      case "textarea":
        return "textbox";
      case "select":
        return element.multiple || element.size > 1 ? "listbox" : "combobox";
      case "option":
        return "option";
      case "img":
        return element.getAttribute("alt") === "" ? null : "img";
      case "h1":
      case "h2":
      case "h3":
      case "h4":
      case "h5":
      case "h6":
        return "heading";
      case "ul":
      case "ol":
      case "menu":
        return "list";
      case "li":
        return "listitem";
      case "nav":
        return "navigation";
      case "main":
        return "main";
      case "header":
        return element.closest("article, aside, main, nav, section") ? null : "banner";
      case "footer":
        return element.closest("article, aside, main, nav, section") ? null : "contentinfo";
      case "aside":
        return "complementary";
      case "form":
        return "form";
      case "table":
        return "table";
      case "tr":
        return "row";
      case "td":
        return "cell";
      case "th":
        return "columnheader";
      case "dialog":
        return "dialog";
      case "fieldset":
      case "details":
        return "group";
      case "summary":
        return "summary";
      case "p":
        return "paragraph";
      case "article":
        return "article";
      case "progress":
        return "progressbar";
      case "iframe":
        return "iframe";
      case "label":
        return "label";
      case "section":
        return element.hasAttribute("aria-label") || element.hasAttribute("aria-labelledby")
          ? "region"
          : null;
      default:
        break;
    }
    if (isEditingHost(element)) {
      return "textbox";
    }
    return null;
  }

  function isEditingHost(element) {
    return (
      !!element.isContentEditable &&
      (!element.parentElement || !element.parentElement.isContentEditable)
    );
  }

  function isInteractive(element, role) {
    var tag = tagOf(element);
    if (tag === "a" && element.hasAttribute("href")) return true;
    if (tag === "button" || tag === "select" || tag === "textarea" || tag === "summary") return true;
    if (tag === "input" && inputType(element) !== "hidden") return true;
    if (isEditingHost(element)) return true;
    if (role && INTERACTIVE_ROLES[role]) return true;
    var tabIndex = element.getAttribute("tabindex");
    if (tabIndex !== null && Number(tabIndex) >= 0) return true;
    try {
      if (typeof element.onclick === "function" || element.hasAttribute("onclick")) return true;
    } catch (error) {
      // Ignored: treated as not interactive.
    }
    return false;
  }

  function isScrollable(element, style) {
    if (!style) return false;
    var overflow = style.overflowY + " " + style.overflowX;
    return (
      /(auto|scroll|overlay)/.test(overflow) &&
      (element.scrollHeight > element.clientHeight + 1 ||
        element.scrollWidth > element.clientWidth + 1)
    );
  }

  function normalizeSpace(text) {
    return String(text === undefined || text === null ? "" : text)
      .replace(/\s+/g, " ")
      .trim();
  }

  function textOf(element) {
    try {
      return normalizeSpace(
        element.innerText !== undefined ? element.innerText : element.textContent
      );
    } catch (error) {
      return "";
    }
  }

  function labelText(element) {
    try {
      var labels = element.labels;
      if (labels && labels.length) {
        return normalizeSpace(
          Array.prototype.map
            .call(labels, function (label) {
              return textOf(label);
            })
            .join(" ")
        );
      }
    } catch (error) {
      // Ignored: no label.
    }
    return "";
  }

  function accessibleName(element, role) {
    var name = "";
    var labelledBy = element.getAttribute("aria-labelledby");
    if (labelledBy) {
      var ownerDocument = element.ownerDocument;
      name = labelledBy
        .split(/\s+/)
        .map(function (id) {
          var labelElement = ownerDocument.getElementById(id);
          return labelElement ? textOf(labelElement) : "";
        })
        .join(" ");
    }
    if (!normalizeSpace(name)) name = element.getAttribute("aria-label") || "";
    var tag = tagOf(element);
    if (!normalizeSpace(name) && (tag === "input" || tag === "select" || tag === "textarea")) {
      name = labelText(element);
      var type = inputType(element);
      if (!name && (type === "submit" || type === "button" || type === "reset")) {
        name = element.value || (type === "submit" ? "Submit" : type === "reset" ? "Reset" : "");
      }
      if (!name && type === "image") name = element.getAttribute("alt") || "";
      if (!name) name = element.getAttribute("placeholder") || "";
    }
    if (!normalizeSpace(name) && (tag === "img" || tag === "area")) {
      name = element.getAttribute("alt") || "";
    }
    if (!normalizeSpace(name) && NAME_FROM_CONTENT[role]) {
      name = textOf(element);
      if (!name && element.querySelector) {
        var image = element.querySelector("img[alt], svg[aria-label]");
        if (image) name = image.getAttribute("alt") || image.getAttribute("aria-label") || "";
      }
    }
    if (!normalizeSpace(name)) name = element.getAttribute("title") || "";
    return cap(normalizeSpace(name), NAME_LIMIT);
  }

  function stateAttributes(element, role) {
    var parts = [];
    var tag = tagOf(element);
    if (role === "heading") {
      var level =
        element.getAttribute("aria-level") || (/^h[1-6]$/.test(tag) ? tag.charAt(1) : "");
      if (level) parts.push("level=" + level);
    }
    // Shown so that the model knows before it tries that the editor refuses these.
    var sensitive = sensitiveKind(element);
    if (sensitive) parts.push(sensitive);
    if (tag === "input" || tag === "textarea") {
      var type = inputType(element);
      if (type === "checkbox" || type === "radio") {
        if (element.checked) parts.push("checked");
      } else if (type === "password") {
        if (element.value) parts.push("value=(hidden)");
      } else if (element.value && !/^(submit|button|reset)$/.test(type)) {
        parts.push("value=" + jsonStringify(cap(element.value, NAME_LIMIT)));
      }
      if (element.required) parts.push("required");
      if (element.readOnly) parts.push("readonly");
    } else if (tag === "select") {
      var selected = Array.prototype.filter
        .call(element.options, function (option) {
          return option.selected;
        })
        .map(function (option) {
          return normalizeSpace(option.text);
        });
      if (selected.length) {
        parts.push("value=" + jsonStringify(cap(selected.join(", "), NAME_LIMIT)));
      }
    } else if (tag === "option" && element.selected) {
      parts.push("selected");
    }
    var checked = element.getAttribute("aria-checked");
    if (checked && tag !== "input") parts.push(checked === "true" ? "checked" : "checked=" + checked);
    var expanded = element.getAttribute("aria-expanded");
    if (expanded) parts.push(expanded === "true" ? "expanded" : "collapsed");
    if (element.getAttribute("aria-pressed") === "true") parts.push("pressed");
    if (element.getAttribute("aria-selected") === "true") parts.push("selected");
    if (element.disabled || element.getAttribute("aria-disabled") === "true") parts.push("disabled");
    if (tag === "a" && element.getAttribute("href")) {
      parts.push("href=" + jsonStringify(cap(element.getAttribute("href"), NAME_LIMIT)));
    }
    return parts;
  }

  function roleAndName(element, role) {
    var name = accessibleName(element, role);
    return role + (name ? " " + jsonStringify(name) : "");
  }

  function describeElement(element) {
    try {
      var role = roleOf(element) || tagOf(element);
      var text = roleAndName(element, role);
      var state = stateAttributes(element, role).filter(function (part) {
        return part.indexOf("href=") !== 0;
      });
      return state.length ? text + " (" + state.join(", ") + ")" : text;
    } catch (error) {
      return "element";
    }
  }

  // `undefined` when the element itself may be shown; "subtree" when nothing
  // inside it is shown; "self" when only the element itself is left out.
  function hiddenKind(element, style) {
    if (!style) return "subtree";
    if (style.display === "none") return "subtree";
    if (element.getAttribute("aria-hidden") === "true") return "subtree";
    if (style.visibility === "hidden" || style.visibility === "collapse") return "subtree";
    if (style.display === "contents") return "self";
    var rect = element.getBoundingClientRect();
    if (rect.width === 0 && rect.height === 0) return "self";
    return undefined;
  }

  function isVisible(element) {
    try {
      if (typeof element.checkVisibility === "function") {
        if (!element.checkVisibility({ checkVisibilityCSS: true })) return false;
      }
      var style = styleOf(element);
      if (!style || style.display === "none" || style.visibility === "hidden") return false;
      var rect = element.getBoundingClientRect();
      return rect.width > 0 || rect.height > 0;
    } catch (error) {
      return false;
    }
  }

  function sameOriginDocument(frame) {
    try {
      return frame.contentDocument || null;
    } catch (error) {
      return null;
    }
  }

  // The nodes rendered as an element's children: an open shadow root replaces
  // the light children, a slot shows what is assigned to it, and a same-origin
  // frame shows its document.
  function renderedChildren(node) {
    if (node.nodeType === 1) {
      if (node.shadowRoot) return node.shadowRoot.childNodes;
      var tag = tagOf(node);
      if (tag === "slot" && typeof node.assignedNodes === "function") {
        var assigned = node.assignedNodes({ flatten: true });
        if (assigned.length) return assigned;
      }
      if (tag === "iframe") {
        var frameDocument = sameOriginDocument(node);
        return frameDocument && frameDocument.body ? [frameDocument.body] : [];
      }
    }
    return node.childNodes;
  }

  // ---------------------------------------------------------------------------
  // Snapshot, page text, find
  // ---------------------------------------------------------------------------

  var SKIPPED_TAGS = table({ script: true, style: true, noscript: true, template: true, head: true });

  function snapshot(args) {
    var interactiveOnly = !!args.interactiveOnly;
    var maxChars = Number(args.maxChars) || 20000;
    var lines = [];
    var length = 0;
    var truncated = false;

    function emit(depth, text) {
      if (truncated) return;
      var line = new Array(depth + 1).join("  ") + "- " + text;
      if (length + line.length + 1 > maxChars) {
        truncated = true;
        return;
      }
      lines.push(line);
      length += line.length + 1;
    }

    function walk(node, depth) {
      if (truncated) return;
      if (node.nodeType === 3) {
        if (!interactiveOnly) {
          var text = normalizeSpace(node.nodeValue);
          if (text) emit(depth, "text: " + jsonStringify(cap(text, TEXT_NODE_LIMIT)));
        }
        return;
      }
      if (node.nodeType !== 1 && node.nodeType !== 11) return;
      var childDepth = depth;
      if (node.nodeType === 1) {
        var tag = tagOf(node);
        if (SKIPPED_TAGS[tag]) return;
        var style = styleOf(node);
        var hidden = hiddenKind(node, style);
        if (hidden === "subtree") return;
        if (hidden !== "self") {
          var role = roleOf(node);
          var interactive = isInteractive(node, role);
          var scrollable = isScrollable(node, style);
          var shown = interactiveOnly
            ? interactive || scrollable
            : (!!role && role !== "paragraph") || interactive || scrollable;
          if (shown) {
            var name = accessibleName(node, role);
            var line = (role || tag) + (name ? " " + jsonStringify(name) : "");
            var state = stateAttributes(node, role);
            if (state.length) line += " [" + state.join("] [") + "]";
            if (scrollable && !interactive) line += " [scrollable]";
            if (interactive || scrollable) line += " [ref=" + refFor(node) + "]";
            emit(depth, line);
            childDepth = depth + 1;
            if (tag === "select" || tag === "textarea") return;
            // The text of a link or a button is already its name.
            if (
              name &&
              NAME_FROM_CONTENT[role] &&
              !node.querySelector("input, select, textarea, button, a[href]")
            ) {
              return;
            }
          }
        }
      }
      var children = renderedChildren(node);
      for (var index = 0; index < children.length && !truncated; index += 1) {
        walk(children[index], childDepth);
      }
    }

    if (document.body) walk(document.body, 0);
    return {
      title: document.title,
      url: location.href,
      text: lines.join("\n") || "(nothing is shown)",
      truncated: truncated,
    };
  }

  function pageText(args) {
    var maxChars = Number(args.maxChars) || 20000;
    var root = null;
    var candidates = [document.querySelector("main, [role=main]"), document.querySelector("article")];
    for (var index = 0; index < candidates.length; index += 1) {
      if (candidates[index] && textOf(candidates[index])) {
        root = candidates[index];
        break;
      }
    }
    root = root || document.body;
    var raw = "";
    try {
      raw = root ? (root.innerText !== undefined ? root.innerText : root.textContent) : "";
    } catch (error) {
      raw = "";
    }
    var text = String(raw || "")
      .replace(/\r/g, "")
      .split("\n")
      .map(function (line) {
        return line.replace(/[ \t ]+/g, " ").trim();
      })
      .join("\n")
      .replace(/\n{3,}/g, "\n\n")
      .trim();
    var truncated = text.length > maxChars;
    return {
      title: document.title,
      url: location.href,
      text: truncated ? text.slice(0, maxChars) : text,
      truncated: truncated,
    };
  }

  function ownText(element) {
    var text = "";
    for (var child = element.firstChild; child; child = child.nextSibling) {
      if (child.nodeType === 3) text += child.nodeValue;
    }
    return normalizeSpace(text);
  }

  function find(args) {
    var query = normalizeSpace(args.query).toLowerCase();
    var limit = Number(args.limit) || 20;
    if (!query) throw automationError("`query` is empty.");
    var interactiveMatches = [];
    var otherMatches = [];

    function consider(element) {
      var role = roleOf(element);
      var interactive = isInteractive(element, role);
      var haystack = [
        accessibleName(element, role),
        element.getAttribute("placeholder") || "",
        element.getAttribute("title") || "",
        interactive ? textOf(element) : ownText(element),
        tagOf(element) === "input" && inputType(element) !== "password" ? element.value || "" : "",
      ]
        .join("\n")
        .toLowerCase();
      if (haystack.indexOf(query) === -1 || !isVisible(element)) return;
      (interactive ? interactiveMatches : otherMatches).push(element);
    }

    function walk(node) {
      if (node.nodeType === 1) {
        if (SKIPPED_TAGS[tagOf(node)]) return;
        consider(node);
      }
      var children = renderedChildren(node);
      for (var index = 0; index < children.length; index += 1) {
        var child = children[index];
        if (child.nodeType === 1 || child.nodeType === 11) walk(child);
      }
    }

    if (document.body) walk(document.body);
    // An interactive element that contains a matching element stands for it.
    otherMatches = otherMatches.filter(function (element) {
      return !interactiveMatches.some(function (interactive) {
        return interactive.contains(element);
      });
    });
    var all = interactiveMatches.concat(otherMatches);
    return {
      total: all.length,
      matches: all.slice(0, limit).map(function (element) {
        return { ref: refFor(element), description: describeElement(element) };
      }),
    };
  }

  // ---------------------------------------------------------------------------
  // Geometry
  // ---------------------------------------------------------------------------

  // The offset of an element's frame from the top viewport.
  function frameOffset(element) {
    var x = 0;
    var y = 0;
    var view = element.ownerDocument.defaultView;
    while (view && view !== window) {
      var frame = view.frameElement;
      if (!frame) break;
      var rect = frame.getBoundingClientRect();
      x += rect.left + frame.clientLeft;
      y += rect.top + frame.clientTop;
      view = frame.ownerDocument.defaultView;
    }
    return { x: x, y: y };
  }

  // The element at a point of the top viewport, looking into same-origin
  // frames and open shadow roots, with the point in its frame's coordinates.
  function deepElementFromPoint(x, y) {
    var view = window;
    var localX = x;
    var localY = y;
    var element = document.elementFromPoint(x, y);
    for (var guard = 0; element && guard < 32; guard += 1) {
      if (element.shadowRoot && typeof element.shadowRoot.elementFromPoint === "function") {
        var inner = element.shadowRoot.elementFromPoint(localX, localY);
        if (inner && inner !== element) {
          element = inner;
          continue;
        }
      }
      if (tagOf(element) === "iframe") {
        var frameDocument = sameOriginDocument(element);
        if (frameDocument) {
          var rect = element.getBoundingClientRect();
          localX -= rect.left + element.clientLeft;
          localY -= rect.top + element.clientTop;
          view = frameDocument.defaultView;
          element = frameDocument.elementFromPoint(localX, localY);
          continue;
        }
      }
      break;
    }
    return { element: element, view: view, x: localX, y: localY };
  }

  // Activating one of these opens the native file dialog, which the editor
  // refuses to do for the AI.
  function opensFileChooser(element) {
    for (var node = element; node && node.nodeType === 1; node = node.parentElement) {
      if (inputType(node) === "file") return true;
      if (tagOf(node) === "label" && node.control && inputType(node.control) === "file") {
        return true;
      }
    }
    return false;
  }

  function isInViewport(rect, view) {
    return (
      rect.top >= 0 &&
      rect.left >= 0 &&
      rect.bottom <= view.innerHeight &&
      rect.right <= view.innerWidth
    );
  }

  function targetPoint(args) {
    if (args.ref !== undefined && args.ref !== null) {
      var element = resolveRef(args.ref);
      var view = element.ownerDocument.defaultView || window;
      if (!isInViewport(element.getBoundingClientRect(), view)) {
        element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      }
      var rects = element.getClientRects();
      var box = null;
      for (var index = 0; index < rects.length; index += 1) {
        if (rects[index].width > 0 && rects[index].height > 0) {
          box = rects[index];
          break;
        }
      }
      box = box || element.getBoundingClientRect();
      if (box.width === 0 && box.height === 0) {
        throw automationError(
          "The element `" + args.ref + "` has no size on the page; it may be hidden."
        );
      }
      var offset = frameOffset(element);
      var x = offset.x + box.left + box.width / 2;
      var y = offset.y + box.top + box.height / 2;
      var hit = deepElementFromPoint(x, y).element;
      var covered =
        hit && hit !== element && !element.contains(hit) && !hit.contains(element)
          ? describeElement(hit)
          : null;
      return {
        x: x,
        y: y,
        description: describeElement(element) + " [" + args.ref + "]",
        covered: covered,
        opensFileChooser: opensFileChooser(element) || opensFileChooser(hit),
      };
    }
    var pointX = Number(args.x);
    var pointY = Number(args.y);
    if (
      !(pointX >= 0 && pointY >= 0 && pointX < window.innerWidth && pointY < window.innerHeight)
    ) {
      throw automationError(
        "The point (" + args.x + ", " + args.y + ") is outside the viewport, which is " +
          window.innerWidth + "x" + window.innerHeight + " CSS pixels."
      );
    }
    var atPoint = deepElementFromPoint(pointX, pointY).element;
    return {
      x: pointX,
      y: pointY,
      description: atPoint ? describeElement(atPoint) : "the page",
      covered: null,
      opensFileChooser: opensFileChooser(atPoint),
    };
  }

  // ---------------------------------------------------------------------------
  // Synthetic mouse input
  // ---------------------------------------------------------------------------

  var BUTTONS = table({ left: [0, 1], middle: [1, 4], right: [2, 2] });
  var lastHovered = null;

  function modifierFlags(modifiers) {
    var list = modifiers || [];
    return {
      shiftKey: list.indexOf("shift") !== -1,
      ctrlKey: list.indexOf("control") !== -1,
      altKey: list.indexOf("alt") !== -1,
      metaKey: list.indexOf("meta") !== -1,
    };
  }

  function mouseInit(hit, extra) {
    var init = {
      bubbles: true,
      cancelable: true,
      composed: true,
      view: hit.view,
      clientX: hit.x,
      clientY: hit.y,
      screenX: hit.x,
      screenY: hit.y,
    };
    for (var key in extra) {
      if (Object.prototype.hasOwnProperty.call(extra, key)) init[key] = extra[key];
    }
    return init;
  }

  function dispatchMouse(target, type, init) {
    var view = (target.ownerDocument && target.ownerDocument.defaultView) || window;
    var event;
    if (type.indexOf("pointer") === 0 && typeof view.PointerEvent === "function") {
      init.pointerId = 1;
      init.pointerType = "mouse";
      init.isPrimary = true;
      event = new view.PointerEvent(type, init);
    } else {
      event = new view.MouseEvent(type, init);
    }
    return target.dispatchEvent(event);
  }

  function moveTo(hit) {
    var target = hit.element;
    if (!target) return;
    if (lastHovered !== target) {
      var previous = lastHovered && lastHovered.isConnected ? lastHovered : null;
      if (previous) {
        dispatchMouse(previous, "pointerout", mouseInit(hit, { relatedTarget: target }));
        dispatchMouse(previous, "mouseout", mouseInit(hit, { relatedTarget: target }));
        dispatchMouse(previous, "pointerleave", mouseInit(hit, { bubbles: false, relatedTarget: target }));
        dispatchMouse(previous, "mouseleave", mouseInit(hit, { bubbles: false, relatedTarget: target }));
      }
      dispatchMouse(target, "pointerover", mouseInit(hit, { relatedTarget: previous }));
      dispatchMouse(target, "mouseover", mouseInit(hit, { relatedTarget: previous }));
      dispatchMouse(target, "pointerenter", mouseInit(hit, { bubbles: false, relatedTarget: previous }));
      dispatchMouse(target, "mouseenter", mouseInit(hit, { bubbles: false, relatedTarget: previous }));
      lastHovered = target;
    }
    dispatchMouse(target, "pointermove", mouseInit(hit, {}));
    dispatchMouse(target, "mousemove", mouseInit(hit, {}));
  }

  function focusableAncestor(element) {
    var current = element;
    while (current && current.nodeType === 1) {
      if (
        typeof current.focus === "function" &&
        (current.tabIndex >= 0 || current.isContentEditable) &&
        !current.disabled
      ) {
        return current;
      }
      current = current.parentElement;
    }
    return null;
  }

  function click(args) {
    var hit = deepElementFromPoint(Number(args.x), Number(args.y));
    var target = hit.element;
    if (!target) throw automationError("Nothing is at that point of the page.");
    var buttonInfo = BUTTONS[args.button] || BUTTONS.left;
    var flags = modifierFlags(args.modifiers);
    var count = Math.max(1, Math.min(3, Number(args.clickCount) || 1));
    moveTo(hit);
    for (var detail = 1; detail <= count; detail += 1) {
      var down = { button: buttonInfo[0], buttons: buttonInfo[1], detail: detail };
      var up = { button: buttonInfo[0], buttons: 0, detail: detail };
      for (var flag in flags) {
        down[flag] = flags[flag];
        up[flag] = flags[flag];
      }
      dispatchMouse(target, "pointerdown", mouseInit(hit, down));
      var notPrevented = dispatchMouse(target, "mousedown", mouseInit(hit, down));
      if (notPrevented && buttonInfo[0] === 0) {
        var focusTarget = focusableAncestor(target);
        if (focusTarget && focusTarget.ownerDocument.activeElement !== focusTarget) {
          focusTarget.focus();
        }
      }
      dispatchMouse(target, "pointerup", mouseInit(hit, up));
      dispatchMouse(target, "mouseup", mouseInit(hit, up));
      if (buttonInfo[0] === 0) {
        dispatchMouse(target, "click", mouseInit(hit, up));
        if (detail === 2) dispatchMouse(target, "dblclick", mouseInit(hit, up));
      } else {
        dispatchMouse(target, "auxclick", mouseInit(hit, up));
        if (buttonInfo[0] === 2) dispatchMouse(target, "contextmenu", mouseInit(hit, up));
      }
    }
    return { description: describeElement(target) };
  }

  function hover(args) {
    var hit = deepElementFromPoint(Number(args.x), Number(args.y));
    if (!hit.element) throw automationError("Nothing is at that point of the page.");
    moveTo(hit);
    return { description: describeElement(hit.element) };
  }

  // ---------------------------------------------------------------------------
  // Keyboard input and text
  // ---------------------------------------------------------------------------

  var LEGACY_KEY_CODES = table({
    Backspace: 8, Tab: 9, Enter: 13, Escape: 27, " ": 32, PageUp: 33, PageDown: 34,
    End: 35, Home: 36, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40,
    Insert: 45, Delete: 46,
  });

  function legacyKeyCode(key) {
    if (LEGACY_KEY_CODES[key] !== undefined) return LEGACY_KEY_CODES[key];
    if (/^F([1-9]|1[0-9]|2[0-4])$/.test(key)) return 111 + Number(key.slice(1));
    if (key.length === 1) {
      var upper = key.toUpperCase();
      if (/[A-Z0-9]/.test(upper)) return upper.charCodeAt(0);
      return key.charCodeAt(0);
    }
    return 0;
  }

  function activeElementDeep() {
    var element = document.activeElement;
    for (var guard = 0; element && guard < 32; guard += 1) {
      if (tagOf(element) === "iframe") {
        var frameDocument = sameOriginDocument(element);
        if (frameDocument && frameDocument.activeElement) {
          element = frameDocument.activeElement;
          continue;
        }
      }
      if (element.shadowRoot && element.shadowRoot.activeElement) {
        element = element.shadowRoot.activeElement;
        continue;
      }
      break;
    }
    return element || document.body || document.documentElement;
  }

  function isTextField(element) {
    var tag = tagOf(element);
    if (tag === "textarea") return true;
    return tag === "input" && TEXT_INPUT_TYPES[inputType(element)] === true;
  }

  function isEditable(element) {
    if (!element) return false;
    if (isTextField(element)) return !element.readOnly && !element.disabled;
    return !!element.isContentEditable;
  }

  // Sets the value the way a framework's controlled input notices it: through
  // the prototype's setter, not an instance property the framework may shadow.
  function setNativeValue(element, value) {
    var view = element.ownerDocument.defaultView || window;
    var tag = tagOf(element);
    var prototype =
      tag === "textarea"
        ? view.HTMLTextAreaElement.prototype
        : tag === "select"
          ? view.HTMLSelectElement.prototype
          : view.HTMLInputElement.prototype;
    var descriptor = Object.getOwnPropertyDescriptor(prototype, "value");
    if (descriptor && descriptor.set) {
      descriptor.set.call(element, value);
    } else {
      element.value = value;
    }
  }

  function dispatchInput(element, kind, data) {
    var view = element.ownerDocument.defaultView || window;
    var event;
    try {
      event = new view.InputEvent("input", {
        bubbles: true,
        composed: true,
        inputType: kind,
        data: data,
      });
    } catch (error) {
      event = new view.Event("input", { bubbles: true });
    }
    element.dispatchEvent(event);
  }

  function dispatchChange(element) {
    var view = element.ownerDocument.defaultView || window;
    element.dispatchEvent(new view.Event("change", { bubbles: true }));
  }

  function supportsSelection(element) {
    try {
      return typeof element.selectionStart === "number";
    } catch (error) {
      return false;
    }
  }

  function insertText(element, text) {
    if (!isEditable(element)) return false;
    var ownerDocument = element.ownerDocument;
    try {
      if (ownerDocument.execCommand("insertText", false, text)) return true;
    } catch (error) {
      // Fall back to editing the value below.
    }
    if (isTextField(element)) {
      var value = element.value;
      var start = supportsSelection(element) ? element.selectionStart : value.length;
      var end = supportsSelection(element) ? element.selectionEnd : value.length;
      setNativeValue(element, value.slice(0, start) + text + value.slice(end));
      if (supportsSelection(element)) {
        element.setSelectionRange(start + text.length, start + text.length);
      }
      dispatchInput(element, "insertText", text);
      return true;
    }
    element.appendChild(ownerDocument.createTextNode(text));
    dispatchInput(element, "insertText", text);
    return true;
  }

  function deleteText(element, forward) {
    if (!isEditable(element)) return;
    try {
      if (element.ownerDocument.execCommand(forward ? "forwardDelete" : "delete", false)) return;
    } catch (error) {
      // Fall back below.
    }
    if (isTextField(element) && supportsSelection(element)) {
      var value = element.value;
      var start = element.selectionStart;
      var end = element.selectionEnd;
      if (start === end) {
        if (forward) end = Math.min(value.length, end + 1);
        else start = Math.max(0, start - 1);
      }
      setNativeValue(element, value.slice(0, start) + value.slice(end));
      element.setSelectionRange(start, start);
      dispatchInput(element, forward ? "deleteContentForward" : "deleteContentBackward", null);
    }
  }

  function tabbables(ownerDocument) {
    var selector =
      "a[href], area[href], button, input, select, textarea, summary, iframe, [tabindex], " +
      "[contenteditable=''], [contenteditable='true']";
    return Array.prototype.filter.call(ownerDocument.querySelectorAll(selector), function (element) {
      return (
        !element.disabled &&
        element.tabIndex >= 0 &&
        inputType(element) !== "hidden" &&
        isVisible(element)
      );
    });
  }

  function moveFocus(from, backwards) {
    var list = tabbables(from.ownerDocument || document);
    if (!list.length) return;
    var index = list.indexOf(from);
    var next = backwards
      ? list[index <= 0 ? list.length - 1 : index - 1]
      : list[index === -1 || index === list.length - 1 ? 0 : index + 1];
    next.focus();
    if (isTextField(next) && supportsSelection(next)) {
      try {
        next.select();
      } catch (error) {
        // Some input types have no selection.
      }
    }
  }

  function moveCaret(element, key, shift) {
    if (!isTextField(element) || !supportsSelection(element)) return;
    var length = element.value.length;
    var start = element.selectionStart;
    var end = element.selectionEnd;
    var position;
    if (key === "ArrowLeft") position = start === end || shift ? Math.max(0, (shift ? end : start) - 1) : start;
    else if (key === "ArrowRight") position = start === end || shift ? Math.min(length, end + 1) : end;
    else if (key === "Home") position = 0;
    else if (key === "End") position = length;
    else return;
    if (shift) {
      element.setSelectionRange(Math.min(start, position), Math.max(start, position));
    } else {
      element.setSelectionRange(position, position);
    }
  }

  function implicitSubmit(element) {
    var form = element.form;
    if (!form) return;
    if (typeof form.requestSubmit === "function") {
      form.requestSubmit();
      return;
    }
    var submit = form.querySelector("button[type=submit], input[type=submit], button:not([type])");
    if (submit) submit.click();
  }

  function isButtonLike(element) {
    var tag = tagOf(element);
    var role = roleOf(element);
    return (
      tag === "button" ||
      tag === "summary" ||
      /^(button|link|checkbox|radio|switch|tab|menuitem)$/.test(role || "") ||
      (tag === "input" && /^(button|submit|reset|checkbox|radio|image|file)$/.test(inputType(element)))
    );
  }

  function keyboardEvent(element, type, press, flags) {
    var view = element.ownerDocument.defaultView || window;
    var init = {
      key: press.key,
      code: press.code || "",
      bubbles: true,
      cancelable: true,
      composed: true,
      view: view,
    };
    for (var flag in flags) init[flag] = flags[flag];
    var event = new view.KeyboardEvent(type, init);
    var keyCode =
      type === "keypress"
        ? press.key.length === 1
          ? press.key.charCodeAt(0)
          : 13
        : legacyKeyCode(press.key);
    try {
      // Older code still reads these; the constructor cannot set them.
      Object.defineProperty(event, "keyCode", { get: function () { return keyCode; } });
      Object.defineProperty(event, "which", { get: function () { return keyCode; } });
      if (type === "keypress") {
        Object.defineProperty(event, "charCode", { get: function () { return keyCode; } });
      }
    } catch (error) {
      // The legacy fields stay 0.
    }
    return element.dispatchEvent(event);
  }

  function pressKey(args) {
    var element = activeElementDeep();
    var press = { key: String(args.key), code: String(args.code || "") };
    var key = press.key;
    var flags = modifierFlags(args.modifiers);
    var producesText = key.length === 1 && !flags.ctrlKey && !flags.metaKey && !flags.altKey;
    if (producesText) refuseSensitiveField(element);
    var notPrevented = keyboardEvent(element, "keydown", press, flags);
    if (notPrevented) {
      var editable = isEditable(element);
      if (producesText) {
        if (keyboardEvent(element, "keypress", press, flags) && editable) {
          insertText(element, key);
        }
      } else if (key === "Enter") {
        if (keyboardEvent(element, "keypress", press, flags)) {
          if (tagOf(element) === "textarea" || element.isContentEditable) {
            if (editable) insertText(element, "\n");
          } else if (isTextField(element)) {
            implicitSubmit(element);
          } else if (isButtonLike(element) && typeof element.click === "function") {
            element.click();
          }
        }
      } else if (key === "Backspace" || key === "Delete") {
        deleteText(element, key === "Delete");
      } else if (key === "Tab") {
        moveFocus(element, flags.shiftKey);
      } else if ((flags.ctrlKey || flags.metaKey) && key.toLowerCase() === "a") {
        if (isTextField(element) && typeof element.select === "function") {
          element.select();
        } else {
          try {
            element.ownerDocument.execCommand("selectAll", false);
          } catch (error) {
            // Nothing to select.
          }
        }
      } else if (key === "ArrowLeft" || key === "ArrowRight" || key === "Home" || key === "End") {
        moveCaret(element, key, flags.shiftKey);
      }
    }
    var focusedAfter = activeElementDeep();
    keyboardEvent(focusedAfter.isConnected ? focusedAfter : element, "keyup", press, flags);
    // A space activates a focused button the way a real key press does: on key up.
    if (
      notPrevented &&
      key === " " &&
      !isEditable(element) &&
      isButtonLike(element) &&
      typeof element.click === "function"
    ) {
      element.click();
    }
    return { defaultPrevented: !notPrevented, description: describeElement(element) };
  }

  function codeForCharacter(character) {
    if (/^[a-z]$/i.test(character)) return "Key" + character.toUpperCase();
    if (/^[0-9]$/.test(character)) return "Digit" + character;
    if (character === " ") return "Space";
    return "";
  }

  function typeText(args) {
    var text = String(args.text);
    var element = activeElementDeep();
    var characters = Array.from(text);
    // Very long texts go in at once; per-character events would take seconds
    // and add nothing for a debugging session.
    if (characters.length > 2000) {
      refuseSensitiveField(element);
      if (!insertText(element, text)) {
        throw automationError(
          "The focused element (" + describeElement(element) + ") does not take text."
        );
      }
      return { description: describeElement(element) };
    }
    characters.forEach(function (character) {
      if (character === "\n") {
        pressKey({ key: "Enter", code: "Enter", modifiers: [] });
        return;
      }
      var target = activeElementDeep();
      refuseSensitiveField(target);
      var press = { key: character, code: codeForCharacter(character) };
      var flags = modifierFlags([]);
      if (
        keyboardEvent(target, "keydown", press, flags) &&
        keyboardEvent(target, "keypress", press, flags)
      ) {
        insertText(target, character);
      }
      keyboardEvent(target, "keyup", press, flags);
    });
    return { description: describeElement(element) };
  }

  // ---------------------------------------------------------------------------
  // Focus, fields and values
  // ---------------------------------------------------------------------------

  function focusElement(args) {
    var element = resolveRef(args.ref);
    var wasFocused = activeElementDeep() === element;
    var view = element.ownerDocument.defaultView || window;
    if (!isInViewport(element.getBoundingClientRect(), view)) {
      element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    }
    if (typeof element.focus === "function") element.focus({ preventScroll: true });
    var focused = activeElementDeep();
    if (focused !== element && !element.contains(focused)) {
      throw automationError(
        "The element `" + args.ref + "` (" + describeElement(element) + ") cannot take focus."
      );
    }
    if (!wasFocused && args.caretAtEnd) {
      if (isTextField(element) && supportsSelection(element)) {
        var length = element.value.length;
        element.setSelectionRange(length, length);
      } else if (element.isContentEditable) {
        var selection = element.ownerDocument.getSelection();
        var range = element.ownerDocument.createRange();
        range.selectNodeContents(element);
        range.collapse(false);
        selection.removeAllRanges();
        selection.addRange(range);
      }
    }
    return { description: describeElement(element) };
  }

  function isOpaqueFrame(element) {
    var tag = tagOf(element);
    return (tag === "iframe" || tag === "frame") && !sameOriginDocument(element);
  }

  // "password", "payment" or "" for the fields the editor refuses to type into
  // (`is_sensitive_field`).
  function sensitiveKind(element) {
    if (!element || !element.getAttribute) return "";
    if (inputType(element) === "password") return "password";
    var tokens = String(element.getAttribute("autocomplete") || "")
      .toLowerCase()
      .split(/\s+/);
    if (
      tokens.some(function (token) {
        return token.indexOf("password") !== -1;
      })
    ) {
      return "password";
    }
    if (
      tokens.some(function (token) {
        return token.indexOf("cc-") === 0;
      })
    ) {
      return "payment";
    }
    return "";
  }

  // The same check as the editor's, made again here because the focus can move
  // while text is typed in (a page that jumps to the next field by itself).
  function refuseSensitiveField(element) {
    if (!element || !element.getAttribute) return;
    if (sensitiveKind(element) || isOpaqueFrame(element)) {
      throw automationError("Typing into password or payment fields is not allowed.");
    }
  }

  function field(args) {
    var element =
      args.ref !== undefined && args.ref !== null ? resolveRef(args.ref) : activeElementDeep();
    return {
      tag: tagOf(element),
      type: inputType(element),
      autocomplete: element.getAttribute ? element.getAttribute("autocomplete") || "" : "",
      description: describeElement(element),
      crossOriginFrame: isOpaqueFrame(element),
      opensFileChooser: opensFileChooser(element),
    };
  }

  function truthy(value) {
    if (typeof value === "boolean") return value;
    if (typeof value === "number") return value !== 0;
    if (typeof value === "string") {
      if (/^(true|on|yes|1|checked)$/i.test(value.trim())) return true;
      if (/^(false|off|no|0|unchecked)?$/i.test(value.trim())) return false;
    }
    throw automationError("Give true or false for a checkbox or a radio button.");
  }

  function setValue(args) {
    var element = resolveRef(args.ref);
    var value = args.value;
    var tag = tagOf(element);
    var label = "The element `" + args.ref + "` (" + describeElement(element) + ")";
    // Without its state: the value in it is the one being replaced.
    var name = roleAndName(element, roleOf(element) || tag);
    if (element.disabled) {
      throw automationError(label + " is disabled.");
    }
    if (tag === "select") {
      var wanted = Array.isArray(value) ? value.map(String) : [String(value)];
      if (wanted.length > 1 && !element.multiple) {
        throw automationError("This select takes one value, not a list.");
      }
      var options = Array.prototype.slice.call(element.options);
      var optionTexts = function (selected) {
        return selected.map(function (option) {
          return normalizeSpace(option.text);
        });
      };
      var previousOptions = optionTexts(
        options.filter(function (option) {
          return option.selected;
        })
      );
      var chosen = wanted.map(function (text) {
        var match = options.filter(function (option) {
          return (
            option.value === text ||
            normalizeSpace(option.text) === normalizeSpace(text) ||
            option.label === text
          );
        })[0];
        if (!match) {
          throw automationError(
            "The select has no option " + jsonStringify(text) + ". Its options are: " +
              options
                .map(function (option) {
                  return jsonStringify(normalizeSpace(option.text));
                })
                .join(", ")
          );
        }
        return match;
      });
      options.forEach(function (option) {
        option.selected = chosen.indexOf(option) !== -1;
      });
      dispatchInput(element, "insertReplacementText", null);
      dispatchChange(element);
      return { element: name, previous: previousOptions, value: optionTexts(chosen) };
    }
    var type = inputType(element);
    if (type === "checkbox" || type === "radio") {
      var checked = truthy(value);
      var wasChecked = element.checked;
      if (type === "radio" && !checked) {
        throw automationError(
          "A radio button cannot be cleared; select another one of its group instead."
        );
      }
      if (element.checked !== checked) {
        element.click();
      }
      if (element.checked !== checked) {
        element.checked = checked;
        dispatchInput(element, "insertReplacementText", null);
        dispatchChange(element);
      }
      return { element: name, previous: wasChecked, value: element.checked };
    }
    if (isTextField(element) || /^(range|color)$/.test(type)) {
      if (element.readOnly) {
        throw automationError(label + " is read-only.");
      }
      var previousText = cap(element.value, NAME_LIMIT);
      if (typeof element.focus === "function") element.focus({ preventScroll: true });
      setNativeValue(element, value === null || value === undefined ? "" : String(value));
      dispatchInput(element, "insertReplacementText", null);
      dispatchChange(element);
      return { element: name, previous: previousText, value: element.value };
    }
    if (element.isContentEditable) {
      var previousContent = cap(textOf(element), NAME_LIMIT);
      element.focus({ preventScroll: true });
      var ownerDocument = element.ownerDocument;
      var selection = ownerDocument.getSelection();
      var range = ownerDocument.createRange();
      range.selectNodeContents(element);
      selection.removeAllRanges();
      selection.addRange(range);
      var replaced = false;
      try {
        replaced = ownerDocument.execCommand("insertText", false, String(value));
      } catch (error) {
        replaced = false;
      }
      if (!replaced) {
        element.textContent = String(value);
        dispatchInput(element, "insertReplacementText", null);
      }
      return { element: name, previous: previousContent, value: textOf(element) };
    }
    throw automationError(label + " is not a form control whose value can be set.");
  }

  // ---------------------------------------------------------------------------
  // Scrolling
  // ---------------------------------------------------------------------------

  function scrollPosition() {
    var root = document.scrollingElement || document.documentElement;
    return {
      scrollX: window.scrollX,
      scrollY: window.scrollY,
      scrollWidth: root ? root.scrollWidth : 0,
      scrollHeight: root ? root.scrollHeight : 0,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
    };
  }

  function scrollableAncestor(element, deltaX, deltaY) {
    var current = element;
    while (current && current.nodeType === 1) {
      var style = styleOf(current);
      if (style && /(auto|scroll|overlay)/.test(style.overflowY + " " + style.overflowX)) {
        var canScrollY = deltaY !== 0 && current.scrollHeight > current.clientHeight + 1;
        var canScrollX = deltaX !== 0 && current.scrollWidth > current.clientWidth + 1;
        if (canScrollX || canScrollY) return current;
      }
      var root = current.getRootNode ? current.getRootNode() : null;
      current = current.parentElement || (root && root.host) || null;
    }
    return null;
  }

  function scroll(args) {
    var deltaX = Number(args.deltaX) || 0;
    var deltaY = Number(args.deltaY) || 0;
    if (args.x === undefined || args.x === null) {
      window.scrollBy({ left: deltaX, top: deltaY, behavior: "instant" });
      return scrollPosition();
    }
    var hit = deepElementFromPoint(Number(args.x), Number(args.y));
    var target = hit.element || document.documentElement;
    var view = hit.view || window;
    var notPrevented = target.dispatchEvent(
      new view.WheelEvent("wheel", mouseInit(hit, { deltaX: deltaX, deltaY: deltaY, deltaMode: 0 }))
    );
    if (notPrevented) {
      var scroller = scrollableAncestor(target, deltaX, deltaY);
      if (scroller) {
        scroller.scrollBy({ left: deltaX, top: deltaY, behavior: "instant" });
      } else {
        view.scrollBy({ left: deltaX, top: deltaY, behavior: "instant" });
      }
    }
    return scrollPosition();
  }

  // ---------------------------------------------------------------------------
  // Logs, waiting, viewport
  // ---------------------------------------------------------------------------

  function consoleLog(args) {
    var result = { entries: consoleEntries.slice(), dropped: consoleDropped };
    if (args.clear) {
      consoleEntries.length = 0;
      consoleDropped = 0;
    }
    return result;
  }

  function networkSummary(entry) {
    return {
      id: entry.id,
      kind: entry.kind,
      method: entry.method,
      url: entry.url,
      status: entry.status,
      statusText: entry.statusText,
      error: entry.error,
      pending: entry.pending,
      duration: entry.duration,
      contentType: entry.contentType,
    };
  }

  function networkLog(args) {
    var result = { entries: networkEntries.map(networkSummary), dropped: networkDropped };
    if (args.clear) {
      networkEntries.length = 0;
      networkDropped = 0;
    }
    return result;
  }

  function networkEntry(args) {
    for (var index = 0; index < networkEntries.length; index += 1) {
      if (networkEntries[index].id === String(args.id)) return networkEntries[index];
    }
    return null;
  }

  function waitCheck(args) {
    var result = {};
    if (args.selector !== undefined && args.selector !== null) {
      try {
        result.selector = !!document.querySelector(String(args.selector));
      } catch (error) {
        return { invalidSelector: errorMessage(error) };
      }
    }
    if (args.text !== undefined && args.text !== null) {
      var haystack = "";
      try {
        haystack = normalizeSpace(document.body ? document.body.innerText : "");
      } catch (error) {
        haystack = "";
      }
      result.text = haystack.indexOf(normalizeSpace(args.text)) !== -1;
    }
    return result;
  }

  function viewport() {
    return {
      width: window.innerWidth,
      height: window.innerHeight,
      devicePixelRatio: window.devicePixelRatio || 1,
      url: location.href,
      title: document.title,
    };
  }

  // ---------------------------------------------------------------------------
  // Evaluation results
  // ---------------------------------------------------------------------------

  var pendingEvaluations = new Map();
  var nextEvaluation = 1;

  function describeNode(node) {
    if (node.nodeType === 1) return "[" + describeElement(node) + "]";
    if (node.nodeType === 3) return "[text " + jsonStringify(cap(node.nodeValue, NAME_LIMIT)) + "]";
    if (node.nodeType === 9) return "[document " + (node.URL || "") + "]";
    return "[node]";
  }

  function safeJson(value, limit) {
    var seen = new WeakSet();
    var json;
    try {
      json = jsonStringify(value, function (key, item) {
        if (typeof item === "bigint") return item.toString() + "n";
        if (typeof item === "function") return "[Function " + (item.name || "anonymous") + "]";
        if (typeof item === "symbol") return item.toString();
        if (item === undefined && key !== "") return "[undefined]";
        if (item && typeof item === "object") {
          if (item === window) return "[Window]";
          if (typeof item.nodeType === "number" && typeof item.nodeName === "string") {
            return describeNode(item);
          }
          if (item instanceof Error) {
            return { name: item.name, message: item.message, stack: item.stack };
          }
          if (item instanceof Map) return { "[Map]": Array.from(item.entries()) };
          if (item instanceof Set) return { "[Set]": Array.from(item.values()) };
          if (seen.has(item)) return "[Circular]";
          seen.add(item);
        }
        return item;
      });
    } catch (error) {
      json = jsonStringify("[unserializable: " + errorMessage(error) + "]");
    }
    if (json === undefined) {
      return { json: undefined, truncated: false };
    }
    var truncated = json.length > limit;
    return { json: truncated ? json.slice(0, limit) : json, truncated: truncated };
  }

  function serialize(value) {
    if (value === undefined) return { type: "undefined" };
    var type = value === null ? "null" : Array.isArray(value) ? "array" : typeof value;
    var result = safeJson(value, EVAL_LIMIT);
    return {
      type: type,
      json: result.json === undefined ? String(value) : result.json,
      truncated: result.truncated,
    };
  }

  function errorEnvelope(error) {
    return { ok: false, error: errorMessage(error), stack: errorStack(error) };
  }

  function evalValue(value) {
    try {
      if (
        value &&
        (typeof value === "object" || typeof value === "function") &&
        typeof value.then === "function"
      ) {
        var id = nextEvaluation;
        nextEvaluation += 1;
        pendingEvaluations.set(id, { state: "pending" });
        // Results nobody asked for (the command timed out) are dropped, oldest first.
        while (pendingEvaluations.size > PENDING_EVALUATION_LIMIT) {
          pendingEvaluations.delete(pendingEvaluations.keys().next().value);
        }
        Promise.resolve(value).then(
          function (result) {
            var serialized = serialize(result);
            serialized.state = "done";
            pendingEvaluations.set(id, serialized);
          },
          function (error) {
            pendingEvaluations.set(id, {
              state: "rejected",
              error: errorMessage(error),
              stack: errorStack(error),
            });
          }
        );
        return { ok: true, value: { pending: id } };
      }
      return { ok: true, value: serialize(value) };
    } catch (error) {
      return errorEnvelope(error);
    }
  }

  function evalResult(args) {
    var id = Number(args.id);
    var state = pendingEvaluations.get(id);
    if (!state) throw automationError("The evaluation was lost; the page may have navigated.");
    if (state.state === "pending") return { state: "pending" };
    pendingEvaluations.delete(id);
    if (state.state === "rejected") return { error: state.error, stack: state.stack };
    return { type: state.type, json: state.json, truncated: state.truncated };
  }

  // ---------------------------------------------------------------------------
  // The namespace
  // ---------------------------------------------------------------------------

  var handlers = {
    pageText: pageText,
    snapshot: snapshot,
    find: find,
    targetPoint: targetPoint,
    click: click,
    hover: hover,
    focus: focusElement,
    field: field,
    typeText: typeText,
    pressKey: pressKey,
    scroll: scroll,
    scrollPosition: scrollPosition,
    setValue: setValue,
    console: consoleLog,
    network: networkLog,
    networkEntry: networkEntry,
    waitCheck: waitCheck,
    viewport: viewport,
    evalResult: evalResult,
  };

  function call(name, args) {
    var handler = Object.prototype.hasOwnProperty.call(handlers, name) ? handlers[name] : null;
    if (!handler) {
      return { ok: false, error: "unknown browser automation function " + String(name), stack: "" };
    }
    try {
      return { ok: true, value: handler(args || {}) };
    } catch (error) {
      if (error && error.automation) {
        return { ok: false, error: String(error.message), stack: "" };
      }
      return errorEnvelope(error);
    }
  }

  try {
    Object.defineProperty(window, NAMESPACE, {
      value: Object.freeze({ call: call, evalValue: evalValue, errorEnvelope: errorEnvelope }),
      enumerable: false,
      configurable: false,
      writable: false,
    });
  } catch (error) {
    // Someone defined the name first; the editor reports the page as not instrumented.
  }
})();

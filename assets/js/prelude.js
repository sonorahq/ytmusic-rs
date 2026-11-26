if (typeof globalThis.structuredClone === "undefined") {
  globalThis.structuredClone = function (value) {
    return value === undefined ? undefined : JSON.parse(JSON.stringify(value));
  };
}
if (typeof globalThis.require === "undefined") {
  globalThis.require = function (name) {
    return globalThis[name];
  };
}

// Runs inside a short-lived Chromium page. This intentionally executes the
// server-provided Sentinel program against a real browser environment instead
// of fabricating an attestation result in Rust. Keep it isolated: the private
// compatibility protocol may change independently of Harness.
const input = JSON.parse(atob("__HARNESS_SENTINEL_INPUT__"));

function encode(value) {
  return btoa(String.fromCharCode(...new TextEncoder().encode(JSON.stringify(value))));
}

function randomItem(values) {
  return values[Math.floor(Math.random() * values.length)];
}

function browserUuid() {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (character) => {
    const random = Math.floor(Math.random() * 16);
    const value = character === "x" ? random : (random & 0x3) | 0x8;
    return value.toString(16);
  });
}

function navigatorProbe() {
  const prototype = Object.getPrototypeOf(navigator);
  const key = randomItem(Object.keys(prototype));
  try {
    return `${key}−${String(navigator[key])}`;
  } catch {
    return key;
  }
}

function environmentData() {
  const memory = performance.memory;
  const scripts = Array.from(document.scripts || [])
    .map((script) => script?.src)
    .filter(Boolean);
  return [
    screen?.width + screen?.height,
    String(new Date()),
    memory?.jsHeapSizeLimit ?? null,
    Math.random(),
    navigator.userAgent,
    randomItem(scripts),
    (scripts.map((source) => source?.match("c/[^/]*/_")).filter((match) => match?.length)[0] ?? [])[0]
      ?? document.documentElement.getAttribute("data-build"),
    navigator.language,
    navigator.languages?.join(","),
    Math.random(),
    navigatorProbe(),
    randomItem(Object.keys(document)),
    randomItem(Object.keys(window)),
    performance.now(),
    browserUuid(),
    [...new URLSearchParams(window.location.search).keys()].join(","),
    navigator.hardwareConcurrency,
    performance.timeOrigin,
    Number("ai" in window),
    Number("createPRNG" in window),
    Number("cache" in window),
    Number("data" in window),
    Number("solana" in window),
    Number("dump" in window),
    Number("InstallTrigger" in window),
  ];
}

function requirementsKey() {
  const started = performance.now();
  try {
    const values = environmentData();
    values[3] = 1;
    values[9] = performance.now() - started;
    return `gAAAAAC${encode(values)}`;
  } catch (error) {
    return `gAAAAACwQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D${encode(String(error))}`;
  }
}

function sentinelHash(value) {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619) >>> 0;
  }
  hash ^= hash >>> 16;
  hash = Math.imul(hash, 2246822507) >>> 0;
  hash ^= hash >>> 13;
  hash = Math.imul(hash, 3266489909) >>> 0;
  hash ^= hash >>> 16;
  return (hash >>> 0).toString(16).padStart(8, "0");
}

function proofOfWork(seed, difficulty) {
  const started = performance.now();
  try {
    const values = environmentData();
    for (let nonce = 0; nonce < 500000; nonce += 1) {
      values[3] = nonce;
      values[9] = Math.round(performance.now() - started);
      const candidate = encode(values);
      if (sentinelHash(`${seed}${candidate}`).substring(0, difficulty.length) <= difficulty) {
        return `gAAAAAB${candidate}~S`;
      }
    }
  } catch (error) {
    return `gAAAAABwQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D${encode(String(error))}`;
  }
  return `gAAAAABwQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D${encode("e")}`;
}

let serializedVm = Promise.resolve();

function solveVm(encodedProgram, key) {
  const run = () => new Promise((resolve, reject) => {
    const values = new Map();
    let queue = [];
    let operations = 0;
    let settled = false;

    const runQueue = async () => {
      while (queue.length > 0) {
        const [opcode, ...args] = queue.shift() ?? [];
        const result = values.get(opcode)?.(...args);
        if (result && typeof result.then === "function") await Promise.resolve(result);
        operations += 1;
      }
    };

    const xor = (left, right) => {
      let result = "";
      for (let index = 0; index < left.length; index += 1) {
        result += String.fromCharCode(left.charCodeAt(index) ^ right.charCodeAt(index % right.length));
      }
      return result;
    };

    const initialize = () => {
      values.clear();
      values.set(0, (program) => solveVm(program, String(values.get(16))));
      values.set(1, (target, left) => values.set(target, xor(String(values.get(target)), String(values.get(left)))));
      values.set(2, (target, literal) => values.set(target, literal));
      values.set(5, (target, source) => {
        const value = values.get(target);
        if (Array.isArray(value)) value.push(values.get(source));
        else values.set(target, value + values.get(source));
      });
      values.set(27, (target, source) => {
        const value = values.get(target);
        if (Array.isArray(value)) value.splice(value.indexOf(values.get(source)), 1);
        else values.set(target, value - values.get(source));
      });
      values.set(29, (target, left, right) => values.set(target, Number(values.get(left)) < Number(values.get(right))));
      values.set(33, (target, left, right) => values.set(target, Number(values.get(left)) * Number(values.get(right))));
      values.set(35, (target, left, right) => {
        const divisor = Number(values.get(right));
        values.set(target, divisor === 0 ? 0 : Number(values.get(left)) / divisor);
      });
      values.set(6, (target, object, property) => values.set(target, values.get(object)[String(values.get(property))]));
      values.set(7, (fn, ...args) => values.get(fn)(...args.map((arg) => values.get(arg))));
      values.set(17, (target, fn, ...args) => {
        try {
          const result = values.get(fn)(...args.map((arg) => values.get(arg)));
          if (result && typeof result.then === "function") {
            return result.then((value) => values.set(target, value)).catch((error) => values.set(target, String(error)));
          }
          values.set(target, result);
        } catch (error) {
          values.set(target, String(error));
        }
      });
      values.set(13, (target, fn, ...args) => {
        try {
          values.get(fn)(...args);
        } catch (error) {
          values.set(target, String(error));
        }
      });
      values.set(8, (target, source) => values.set(target, values.get(source)));
      values.set(10, window);
      values.set(11, (target, pattern) => values.set(target,
        (Array.from(document.scripts || [])
          .map((script) => script?.src?.match(String(values.get(pattern))))
          .filter((match) => match?.length)[0] ?? [])[0] ?? null));
      values.set(12, (target) => values.set(target, values));
      values.set(14, (target, source) => values.set(target, JSON.parse(String(values.get(source)))));
      values.set(15, (target, source) => values.set(target, JSON.stringify(values.get(source))));
      values.set(18, (target) => values.set(target, atob(String(values.get(target)))));
      values.set(19, (target) => values.set(target, btoa(String(values.get(target)))));
      values.set(20, (left, right, fn, ...args) => values.get(left) === values.get(right) ? values.get(fn)(...args) : null);
      values.set(21, (left, right, limit, fn, ...args) =>
        Math.abs(Number(values.get(left)) - Number(values.get(right))) > Number(values.get(limit))
          ? values.get(fn)(...args)
          : null);
      values.set(23, (value, fn, ...args) => values.get(value) === undefined ? null : values.get(fn)(...args));
      values.set(24, (target, object, property) => {
        const receiver = values.get(object);
        values.set(target, receiver[String(values.get(property))].bind(receiver));
      });
      values.set(34, (target, source) => Promise.resolve(values.get(source)).then((value) => values.set(target, value)));
      values.set(22, (target, nextQueue) => {
        const previous = [...queue];
        queue = [...nextQueue];
        return runQueue().catch((error) => values.set(target, String(error))).finally(() => { queue = previous; });
      });
      values.set(28, () => undefined);
      values.set(26, () => undefined);
      values.set(25, () => undefined);
      values.set(30, (target, returnSlot, argumentSlots, bodyOrLegacy, maybeBody) => {
        const hasArguments = Array.isArray(maybeBody);
        const slots = hasArguments ? bodyOrLegacy : [];
        const body = (hasArguments ? maybeBody : bodyOrLegacy) ?? [];
        values.set(target, (...args) => {
          if (settled) return;
          const previous = [...queue];
          if (hasArguments) {
            for (let index = 0; index < slots.length; index += 1) values.set(slots[index], args[index]);
          }
          queue = [...body];
          return runQueue().then(() => values.get(returnSlot)).catch((error) => String(error)).finally(() => { queue = previous; });
        });
      });
      values.set(3, (value) => {
        if (!settled) {
          settled = true;
          resolve(btoa(String(value)));
        }
      });
      values.set(4, (value) => {
        if (!settled) {
          settled = true;
          reject(new Error(btoa(String(value))));
        }
      });
      values.set(16, key);
    };

    initialize();
    setTimeout(() => {
      if (!settled) {
        settled = true;
        resolve(String(operations));
      }
    }, 500);
    try {
      queue = JSON.parse(xor(atob(encodedProgram), String(values.get(16))));
      runQueue().catch((error) => resolve(btoa(`${operations}: ${String(error)}`)));
    } catch (error) {
      resolve(btoa(`${operations}: ${String(error)}`));
    }
  });
  const result = serializedVm.then(run, run);
  serializedVm = result.then(() => undefined, () => undefined);
  return result;
}

await (async () => {
  if (input.mode === "requirements-key") {
    document.body.textContent = JSON.stringify({ requirements_key: requirementsKey() });
    return;
  }
  const requirements = input.requirements ?? {};
  const proof = requirements.proofofwork?.required
    ? proofOfWork(requirements.proofofwork.seed, requirements.proofofwork.difficulty)
    : null;
  const turnstile = requirements.turnstile?.required
    ? await solveVm(requirements.turnstile.dx, input.requirements_key)
    : null;
  document.body.textContent = JSON.stringify({ proof, turnstile });
})().catch((error) => {
  document.body.textContent = JSON.stringify({ error: String(error) });
});

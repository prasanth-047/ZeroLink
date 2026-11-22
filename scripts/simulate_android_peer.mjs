/**
 * ZeroLink Android peer simulator (Node.js, no third-party packages).
 * Mirrors the v3 exchange and synthetic packets from simulate_android_peer.py.
 * The Windows first-pair approval still requires a person to compare the code.
 */
import net from "node:net";
import path from "node:path";
import { readFile, writeFile } from "node:fs/promises";
import {
  createCipheriv,
  createHash,
  createHmac,
  createPublicKey,
  createDecipheriv,
  diffieHellman,
  generateKeyPairSync,
  hkdfSync,
  randomBytes,
} from "node:crypto";
import { createInterface } from "node:readline/promises";
import { setTimeout as delay } from "node:timers/promises";

const MAGIC = 0x5a4c;
const VERSION = 1;
const HEADER_SIZE = 20;
const PKT_HANDSHAKE = 1;
const PKT_SIGNALING = 2;
const PKT_NOTIFICATION = 3;
const PKT_CLIPBOARD = 4;
const PKT_FILE_METADATA = 5;
const PKT_WIFI_WAKE = 7;
const PKT_HEARTBEAT = 9;
const X25519_SPKI_PREFIX = Buffer.from("302a300506032b656e032100", "hex");
const HYDRATION_FILE_ID = "770001";
const HYDRATION_RELATIVE_PATH = "Verification/ZeroLink-Hydration-Test.bin";
const HYDRATION_FILE_SIZE = 4 * 1024 * 1024;
const BULK_REQUEST_AAD_LABEL = Buffer.from("ZeroLink-v1-wifi-bulk-request");
const BULK_RESPONSE_AAD_LABEL = Buffer.from("ZeroLink-v1-wifi-bulk-response");

const crcTable = Uint32Array.from({ length: 256 }, (_, value) => {
  let crc = value;
  for (let bit = 0; bit < 8; bit++) {
    crc = (crc & 1) !== 0 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
  }
  return crc >>> 0;
});

function crc32(data) {
  let crc = 0xffffffff;
  for (const byte of data) crc = (crc >>> 8) ^ crcTable[(crc ^ byte) & 0xff];
  return (crc ^ 0xffffffff) >>> 0;
}

function buildFrame(packetType, sequence, payload) {
  const header = Buffer.alloc(HEADER_SIZE);
  header.writeUInt16BE(MAGIC, 0);
  header.writeUInt8(VERSION, 2);
  header.writeUInt8(packetType, 3);
  header.writeUInt32BE(payload.length, 4);
  header.writeBigUInt64BE(BigInt(sequence), 8);
  header.writeUInt32BE(crc32(header.subarray(0, 16)), 16);
  return Buffer.concat([header, payload]);
}

function createFrameReader(socket) {
  let buffer = Buffer.alloc(0);
  let closedError = null;
  const reads = [];

  function drain() {
    while (reads.length > 0 && buffer.length >= reads[0].length) {
      const { length, resolve } = reads.shift();
      resolve(buffer.subarray(0, length));
      buffer = buffer.subarray(length);
    }
    if (closedError) {
      while (reads.length > 0) reads.shift().reject(closedError);
    }
  }

  socket.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    drain();
  });
  socket.on("error", (error) => {
    closedError = error;
    drain();
  });
  socket.on("close", () => {
    closedError ??= new Error("Windows daemon closed the connection");
    drain();
  });

  function readExact(length) {
    return new Promise((resolve, reject) => {
      reads.push({ length, resolve, reject });
      drain();
    });
  }

  return async function receiveFrame() {
    const header = await readExact(HEADER_SIZE);
    const magic = header.readUInt16BE(0);
    const version = header.readUInt8(2);
    const packetType = header.readUInt8(3);
    const payloadLength = header.readUInt32BE(4);
    const sequence = header.readBigUInt64BE(8);
    const expectedCrc = header.readUInt32BE(16);

    if (magic !== MAGIC) throw new Error(`Invalid frame magic: 0x${magic.toString(16)}`);
    if (version !== VERSION) throw new Error(`Unsupported frame version: ${version}`);
    if (crc32(header.subarray(0, 16)) !== expectedCrc) throw new Error("Invalid frame header CRC");
    if (payloadLength > 16 * 1024 * 1024) throw new Error(`Frame too large: ${payloadLength}`);

    return {
      packetType,
      sequence,
      payload: await readExact(payloadLength),
    };
  };
}

function createExactSocketReader(socket) {
  let buffer = Buffer.alloc(0);
  let closedError = null;
  const reads = [];

  function drain() {
    while (reads.length > 0 && buffer.length >= reads[0].length) {
      const { length, resolve } = reads.shift();
      resolve(buffer.subarray(0, length));
      buffer = buffer.subarray(length);
    }
    if (closedError) {
      while (reads.length > 0) reads.shift().reject(closedError);
    }
  }

  socket.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    drain();
  });
  socket.on("error", (error) => {
    closedError = error;
    drain();
  });
  socket.on("close", () => {
    closedError ??= new Error("Bulk transfer socket closed unexpectedly");
    drain();
  });

  return (length) => new Promise((resolve, reject) => {
    reads.push({ length, resolve, reject });
    drain();
  });
}

function decryptFrame(frame, key, ivBase) {
  if (frame.payload.length < 16) throw new Error("Encrypted peer frame is truncated");
  const ciphertext = frame.payload.subarray(0, -16);
  const authTag = frame.payload.subarray(-16);
  const decipher = createDecipheriv("aes-256-gcm", key, deriveNonce(ivBase, frame.sequence));
  decipher.setAAD(Buffer.from([frame.packetType]));
  decipher.setAuthTag(authTag);
  return Buffer.concat([decipher.update(ciphertext), decipher.final()]);
}

function serializeWifiWake(payload) {
  const result = [Buffer.alloc(2)];
  result[0].writeUInt16BE(payload.port);
  for (const value of [payload.targetIp, payload.sessionToken, payload.reason]) {
    const bytes = Buffer.from(value, "utf8");
    if (bytes.length > 0xffff) throw new Error("Wi-Fi wake field exceeds the wire limit");
    const length = Buffer.alloc(2);
    length.writeUInt16BE(bytes.length);
    result.push(length, bytes);
  }
  return Buffer.concat(result);
}

function deserializeWifiWake(payload) {
  if (payload.length < 2) throw new Error("Wi-Fi wake payload is truncated");
  const port = payload.readUInt16BE(0);
  let offset = 2;
  function readString() {
    if (payload.length - offset < 2) throw new Error("Wi-Fi wake string length is truncated");
    const length = payload.readUInt16BE(offset);
    offset += 2;
    if (payload.length - offset < length) throw new Error("Wi-Fi wake string is truncated");
    const value = payload.toString("utf8", offset, offset + length);
    offset += length;
    return value;
  }
  const result = { port, targetIp: readString(), sessionToken: readString(), reason: readString() };
  if (offset !== payload.length) throw new Error("Wi-Fi wake payload has trailing bytes");
  return result;
}

function buildHydrationContent() {
  const content = Buffer.alloc(HYDRATION_FILE_SIZE);
  for (let index = 0; index < content.length; index++) {
    content[index] = (index * 31 + 17) & 0xff;
  }
  return content;
}

function parseBulkRangeRequest(body) {
  if (body.length < 15 || body.readUInt8(0) !== 1) {
    throw new Error("Unsupported encrypted photo-range request");
  }
  const idLength = body.readUInt16BE(1);
  if (idLength < 1 || idLength > 256 || body.length !== 15 + idLength) {
    throw new Error("Invalid encrypted photo-range request length");
  }
  const fileId = body.toString("utf8", 3, 3 + idLength);
  const offset = body.readBigUInt64BE(3 + idLength);
  const length = body.readUInt32BE(11 + idLength);
  if (length < 1 || length > 16 * 1024 * 1024) throw new Error("Invalid requested photo-range length");
  return { fileId, offset, length, body };
}

function deriveBulkKeys(sessionToken, challenge) {
  const token = Buffer.from(sessionToken, "hex");
  if (token.length !== 32) throw new Error("Invalid Wi-Fi bulk session token");
  return Buffer.from(hkdfSync("sha256", token, challenge, Buffer.from("ZeroLink-v1-wifi-bulk-transfer"), 64));
}

function decryptBulkRequest(keys, challenge, nonce, encryptedRequest) {
  if (encryptedRequest.length < 16) throw new Error("Encrypted photo-range request is truncated");
  const decipher = createDecipheriv("aes-256-gcm", keys.subarray(0, 32), nonce);
  decipher.setAAD(Buffer.concat([BULK_REQUEST_AAD_LABEL, challenge]));
  decipher.setAuthTag(encryptedRequest.subarray(-16));
  return Buffer.concat([decipher.update(encryptedRequest.subarray(0, -16)), decipher.final()]);
}

function encryptBulkResponse(keys, challenge, requestBody, nonce, responseBody) {
  const cipher = createCipheriv("aes-256-gcm", keys.subarray(32, 64), nonce);
  cipher.setAAD(Buffer.concat([BULK_RESPONSE_AAD_LABEL, challenge, requestBody]));
  return Buffer.concat([cipher.update(responseBody), cipher.final(), cipher.getAuthTag()]);
}

async function serveBulkRange(wake, content, sendReady) {
  const expectedPort = Number(process.env.ZEROLINK_TEST_BULK_PORT);
  if (!Number.isInteger(expectedPort) || expectedPort < 1 || expectedPort > 65535 || wake.port !== expectedPort) {
    throw new Error("Windows bulk-transfer port did not match the isolated simulator listener");
  }
  if (wake.reason !== "wake") throw new Error("Windows sent an unexpected Wi-Fi wake reason");

  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(wake.port, "127.0.0.1", resolve);
  });
  const clientPromise = new Promise((resolve, reject) => {
    server.once("connection", resolve);
    server.once("error", reject);
  });

  try {
    await sendReady(serializeWifiWake({
      targetIp: "127.0.0.1",
      port: wake.port,
      sessionToken: wake.sessionToken,
      reason: "ready",
    }));
    const client = await clientPromise;
    client.setTimeout(20_000, () => client.destroy(new Error("Bulk range request timed out")));
    const readExact = createExactSocketReader(client);
    const challenge = randomBytes(32);
    const keys = deriveBulkKeys(wake.sessionToken, challenge);
    await writeSocket(client, challenge);

    const requestNonce = await readExact(12);
    const requestLengthBytes = await readExact(4);
    const requestLength = requestLengthBytes.readUInt32BE(0);
    if (requestLength < 16 || requestLength > 1024 * 1024 + 1024) {
      throw new Error("Invalid encrypted photo-range request size");
    }
    const encryptedRequest = await readExact(requestLength);
    const requestBody = decryptBulkRequest(keys, challenge, requestNonce, encryptedRequest);
    const request = parseBulkRangeRequest(requestBody);
    if (request.fileId !== HYDRATION_FILE_ID) throw new Error("Windows requested an unexpected synthetic media ID");
    if (request.offset + BigInt(request.length) > BigInt(content.length)) {
      throw new Error("Windows requested a range beyond the synthetic file size");
    }

    const chunk = content.subarray(Number(request.offset), Number(request.offset) + request.length);
    const responseBody = Buffer.alloc(12 + chunk.length);
    responseBody.writeBigUInt64BE(BigInt(content.length), 0);
    responseBody.writeUInt32BE(chunk.length, 8);
    chunk.copy(responseBody, 12);
    const responseNonce = randomBytes(12);
    const encryptedResponse = encryptBulkResponse(keys, challenge, requestBody, responseNonce, responseBody);
    const response = Buffer.alloc(4 + 12 + 4 + encryptedResponse.length);
    response.writeInt32BE(0, 0);
    responseNonce.copy(response, 4);
    response.writeUInt32BE(encryptedResponse.length, 16);
    encryptedResponse.copy(response, 20);
    await writeSocket(client, response);
    client.end();
    console.log(`[Hydration] Served authenticated range offset=${request.offset}, bytes=${chunk.length}.`);
  } finally {
    server.close();
  }
}

async function performCloudHydrationTest({ receiveFrame, initiatorKey, initiatorIv, sendReady }) {
  const syncRoot = process.env.ZEROLINK_TEST_SYNC_ROOT;
  const auditPath = process.env.ZEROLINK_TEST_HYDRATION_AUDIT;
  if (!syncRoot || !auditPath) {
    throw new Error("Cloud Files hydration test requires an isolated sync root and audit path");
  }

  const expectedContent = buildHydrationContent();
  const testFilePath = path.join(syncRoot, ...HYDRATION_RELATIVE_PATH.split("/"));
  console.log("[Hydration] Opening the synthetic Cloud Files placeholder to trigger FETCH_DATA.");

  const hydrationPromise = readFile(testFilePath).then(
    (data) => ({ data }),
    (error) => ({ error }),
  );
  let timeoutId;
  const timeoutPromise = new Promise((resolve) => {
    timeoutId = setTimeout(() => resolve({ timedOut: true }), 120_000);
  });

  let hydrated;
  try {
    while (hydrated === undefined) {
      const next = await Promise.race([
        receiveFrame().then((frame) => ({ frame }), (error) => ({ readError: error })),
        hydrationPromise.then((result) => ({ hydration: result })),
        timeoutPromise,
      ]);

      if (next.timedOut) throw new Error("Timed out waiting for Cloud Files placeholder hydration");
      if (next.hydration) {
        if (next.hydration.error) throw new Error(`Could not read hydrated placeholder: ${next.hydration.error.message}`);
        hydrated = next.hydration.data;
        break;
      }
      if (next.readError) throw new Error(`Windows signaling connection closed during hydration: ${next.readError.message}`);

      const frame = next.frame;
      const plaintext = decryptFrame(frame, initiatorKey, initiatorIv);
      if (frame.packetType !== PKT_WIFI_WAKE) {
        console.log(`[Hydration] Ignored unrelated authenticated packet type ${frame.packetType}.`);
        continue;
      }
      const wake = deserializeWifiWake(plaintext);
      if (wake.reason !== "wake") throw new Error("Windows sent an unexpected bulk-transfer acknowledgement");
      console.log(`[Hydration] Received authenticated range wake for port ${wake.port}.`);
      await serveBulkRange(wake, expectedContent, sendReady);
    }
  } finally {
    clearTimeout(timeoutId);
  }

  if (!hydrated.equals(expectedContent)) {
    throw new Error("Hydrated placeholder content did not match the synthetic Android media bytes");
  }
  const digest = createHash("sha256").update(hydrated).digest("hex");
  await writeFile(
    auditPath,
    `HydrationVerified=true\nHydratedBytes=${hydrated.length}\nSha256=${digest}\n`,
    "utf8",
  );
  console.log(`[Hydration] Placeholder bytes verified (${hydrated.length} bytes, SHA-256 ${digest}).`);
}

function pairingCode(clientPublicKey, salt, serverPublicKey) {
  const digest = createHash("sha256")
    .update("ZeroLink-v3-pairing-code")
    .update(clientPublicKey)
    .update(salt)
    .update(serverPublicKey)
    .digest();
  return String(digest.readUInt32BE(0) % 1_000_000).padStart(6, "0");
}

async function confirmPairingCode(expectedCode) {
  let enteredCode = process.env.ZEROLINK_SIMULATOR_APPROVED_CODE;
  if (enteredCode === undefined) {
    console.log(`[Pairing] Simulator code: ${expectedCode}`);
    if (!process.stdin.isTTY) {
      throw new Error(
        "Pairing is gated. Compare the code in Windows, then rerun with ZEROLINK_SIMULATOR_APPROVED_CODE set to that exact code.",
      );
    }
    const readline = createInterface({ input: process.stdin, output: process.stdout });
    try {
      enteredCode = await readline.question(
        "Compare the six-digit Windows code, then enter it here to approve this simulator pairing: ",
      );
    } finally {
      readline.close();
    }
  }
  if (enteredCode.trim() !== expectedCode) {
    throw new Error("Pairing code did not match; no Android approval was sent");
  }
  console.log("[Pairing] Displayed code matched; simulator approval is authorized.");
}

function deriveNonce(ivBase, sequence) {
  const nonce = Buffer.from(ivBase);
  const seq = Buffer.alloc(8);
  seq.writeBigUInt64LE(BigInt(sequence));
  for (let index = 0; index < 8; index++) nonce[index] ^= seq[index];
  return nonce;
}

function serializeNotification(id, packageName, appName, title, content) {
  const parts = [Buffer.alloc(9)];
  parts[0].writeBigUInt64BE(BigInt(Date.now()), 0);
  parts[0].writeUInt8(0, 8);
  for (const text of [id, packageName, appName, title, content]) {
    const bytes = Buffer.from(text, "utf8");
    if (bytes.length > 0xffff) throw new Error("Notification field exceeds wire limit");
    const length = Buffer.alloc(2);
    length.writeUInt16BE(bytes.length);
    parts.push(length, bytes);
  }
  return Buffer.concat(parts);
}

function serializeClipboard(text) {
  const bytes = Buffer.from(text, "utf8");
  const prefix = Buffer.alloc(21);
  prefix.writeUInt8(0, 0);
  prefix.writeBigUInt64BE(BigInt(crc32(bytes)), 1);
  prefix.writeBigUInt64BE(BigInt(Date.now()), 9);
  prefix.writeUInt32BE(bytes.length, 17);
  return Buffer.concat([prefix, bytes]);
}

function serializeHeartbeat() {
  const payload = Buffer.alloc(6);
  payload.writeUInt8(89, 0);
  payload.writeUInt8(1, 1);
  payload.writeUInt8(1, 2);
  payload.writeUInt16BE(8443, 3);
  payload.writeUInt8(0, 5);
  return payload;
}

function serializeFileCatalog(syncRoot, files) {
  const root = Buffer.from(syncRoot, "utf8");
  const count = Buffer.alloc(4);
  count.writeUInt32BE(files.length);
  const parts = [Buffer.alloc(2), root, count];
  parts[0].writeUInt16BE(root.length);

  for (const file of files) {
    const itemParts = [Buffer.alloc(17)];
    itemParts[0].writeBigUInt64BE(BigInt(file.size), 0);
    itemParts[0].writeBigUInt64BE(BigInt(file.mtime), 8);
    itemParts[0].writeUInt8(file.isDir ? 1 : 0, 16);
    for (const field of [file.id, file.path, file.mime]) {
      const bytes = Buffer.from(field, "utf8");
      if (bytes.length > 0xffff) throw new Error("Catalog field exceeds wire limit");
      const length = Buffer.alloc(2);
      length.writeUInt16BE(bytes.length);
      itemParts.push(length, bytes);
    }
    const item = Buffer.concat(itemParts);
    const itemLength = Buffer.alloc(4);
    itemLength.writeUInt32BE(item.length);
    parts.push(itemLength, item);
  }
  return Buffer.concat(parts);
}

function writeSocket(socket, data) {
  return new Promise((resolve, reject) => {
    socket.write(data, (error) => (error ? reject(error) : resolve()));
  });
}

async function runSimulation() {
  const host = process.env.ZEROLINK_HOST ?? "127.0.0.1";
  const port = Number(process.env.ZEROLINK_PORT ?? 8989);
  const timeoutMs = Number(process.env.ZEROLINK_SIMULATOR_TIMEOUT_SECONDS ?? 180) * 1000;
  if (!Number.isFinite(port) || port < 1 || port > 65535) throw new Error("Invalid ZEROLINK_PORT");
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) throw new Error("Simulator timeout must be positive");

  console.log("=".repeat(65));
  console.log("   ZeroLink Android Peer Simulator (Node.js)");
  console.log("=".repeat(65));
  console.log(`Connecting to Windows Daemon on ${host}:${port}...`);

  const socket = net.createConnection({ host, port });
  socket.setTimeout(timeoutMs, () => socket.destroy(new Error("Simulator socket timeout")));
  const receiveFrame = createFrameReader(socket);
  try {
    await new Promise((resolve, reject) => {
      socket.once("connect", resolve);
      socket.once("error", reject);
    });
    console.log(`Connected. Waiting up to ${timeoutMs / 1000}s for first-pair approval.`);

    const init = await receiveFrame();
    if (init.packetType !== PKT_HANDSHAKE || init.sequence !== 0n) {
      throw new Error("Expected Windows HandshakeInit");
    }
    const payload = init.payload;
    if (payload.length < 88) throw new Error("HandshakeInit payload too short");
    const clientPublicKey = payload.subarray(0, 32);
    const salt = payload.subarray(32, 64);
    const handshakeVersion = payload.readUInt32BE(64);
    if (handshakeVersion !== 3) throw new Error(`Unsupported handshake version ${handshakeVersion}`);
    const nameLength = payload.readUInt32BE(68);
    if (payload.length !== 72 + nameLength + 16 || !payload.subarray(72 + nameLength).equals(Buffer.alloc(16))) {
      throw new Error("Expected an unpaired Windows test profile (zero PSK proof)");
    }

    const { privateKey, publicKey } = generateKeyPairSync("x25519");
    const serverPublicKey = publicKey.export({ type: "spki", format: "der" }).subarray(-32);
    const code = pairingCode(clientPublicKey, salt, serverPublicKey);
    console.log(`Windows pairing code to compare: ${code}`);

    const clientKey = createPublicKey({
      key: Buffer.concat([X25519_SPKI_PREFIX, clientPublicKey]),
      type: "spki",
      format: "der",
    });
    const sharedKey = diffieHellman({ privateKey, publicKey: clientKey });
    const material = Buffer.from(
      hkdfSync("sha256", sharedKey, salt, Buffer.from("ZeroLink-v3-AEAD-Session-Keys"), 152),
    );
    const initiatorKey = material.subarray(0, 32);
    const initiatorIv = material.subarray(32, 44);
    const responderKey = material.subarray(44, 76);
    const ivBase = material.subarray(76, 88);
    const confirmationKey = material.subarray(88, 120);
    const serverTranscript = Buffer.concat([
      Buffer.from("ZeroLink-v3-server-key-confirmation"),
      clientPublicKey,
      salt,
      serverPublicKey,
    ]);
    const serverTag = createHmac("sha256", confirmationKey).update(serverTranscript).digest().subarray(0, 16);
    const deviceName = Buffer.from("ZeroLink Android Peer Simulator");
    const reply = Buffer.alloc(32 + 16 + 2 + 4 + deviceName.length + 16);
    let offset = 0;
    serverPublicKey.copy(reply, offset); offset += 32;
    serverTag.copy(reply, offset); offset += 16;
    reply.writeUInt16BE(0x1001, offset); offset += 2;
    reply.writeUInt32BE(deviceName.length, offset); offset += 4;
    deviceName.copy(reply, offset); offset += deviceName.length;
    Buffer.alloc(16).copy(reply, offset);
    await writeSocket(socket, buildFrame(PKT_HANDSHAKE, 0, reply));

    await confirmPairingCode(code);

    const confirmation = await receiveFrame();
    const clientTranscript = Buffer.concat([
      Buffer.from("ZeroLink-v3-client-key-confirmation"),
      clientPublicKey,
      salt,
      serverPublicKey,
    ]);
    const expectedClientTag = createHmac("sha256", confirmationKey).update(clientTranscript).digest().subarray(0, 16);
    if (
      confirmation.packetType !== PKT_HANDSHAKE ||
      confirmation.sequence !== 1n ||
      !confirmation.payload.equals(Buffer.concat([Buffer.from([0x02]), expectedClientTag]))
    ) {
      throw new Error("Windows mutual key confirmation did not match");
    }
    console.log("[Step 1] ECDH and mutual transcript key confirmation verified.");

    async function sendEncrypted(packetType, sequence, plaintext) {
      const cipher = createCipheriv("aes-256-gcm", responderKey, deriveNonce(ivBase, sequence));
      cipher.setAAD(Buffer.from([packetType]));
      const ciphertext = Buffer.concat([cipher.update(plaintext), cipher.final(), cipher.getAuthTag()]);
      await writeSocket(socket, buildFrame(packetType, sequence, ciphertext));
    }

    await sendEncrypted(PKT_SIGNALING, 1, Buffer.from([0x50, 0x01]));
    console.log("[Step 2] Sent encrypted first-pair approval.");

    if (process.env.ZEROLINK_SIMULATOR_PAIRING_ONLY === "1") {
      await delay(800);
      console.log("[Step 2] Pairing-only run complete; synthetic feature packets were skipped.");
      return;
    }

    await sendEncrypted(
      PKT_NOTIFICATION,
      2,
      serializeNotification(
        "sim_msg_001",
        "com.whatsapp",
        "WhatsApp",
        "Alex Rivera",
        "Hey! ZeroLink low-power sync is working on Windows!",
      ),
    );
    console.log("[Step 3] Sent synthetic notification.");
    await delay(500);

    let nextSequence = 3;
    if (process.env.ZEROLINK_SIMULATOR_SEND_CLIPBOARD === "1") {
      await sendEncrypted(PKT_CLIPBOARD, nextSequence++, serializeClipboard("https://github.com/prasanth-047/ZeroLink"));
      console.log("[Step 4] Sent synthetic clipboard update.");
      await delay(500);
    } else {
      console.log("[Step 4] Skipped synthetic clipboard update (set ZEROLINK_SIMULATOR_SEND_CLIPBOARD=1 to opt in).");
    }

    await sendEncrypted(PKT_HEARTBEAT, nextSequence++, serializeHeartbeat());
    console.log("[Step 5] Sent synthetic heartbeat.");
    await delay(500);

    const nowSeconds = Math.floor(Date.now() / 1000);
    const hydrationTestEnabled = Boolean(
      process.env.ZEROLINK_TEST_SYNC_ROOT && process.env.ZEROLINK_TEST_HYDRATION_AUDIT,
    );
    const catalogFiles = [
      {
        id: "photo_sunset_001",
        path: "Camera/IMG_20261103_Sunset.jpg",
        size: 3_452_100,
        mtime: nowSeconds,
        mime: "image/jpeg",
      },
      {
        id: "photo_workspace_002",
        path: "Camera/IMG_20261103_ZeroLinkSetup.jpg",
        size: 4_891_020,
        mtime: nowSeconds,
        mime: "image/jpeg",
      },
      {
        id: "doc_presentation_003",
        path: "Documents/ZeroLink_Architecture_Notes.pdf",
        size: 842_310,
        mtime: nowSeconds,
        mime: "application/pdf",
      },
    ];
    if (hydrationTestEnabled) {
      catalogFiles.push({
        id: HYDRATION_FILE_ID,
        path: HYDRATION_RELATIVE_PATH,
        size: HYDRATION_FILE_SIZE,
        mtime: nowSeconds,
        mime: "application/octet-stream",
      });
    }
    await sendEncrypted(
      PKT_FILE_METADATA,
      nextSequence++,
      serializeFileCatalog("Android_DCIM", catalogFiles),
    );
    console.log(`[Step 6] Sent a ${catalogFiles.length}-item synthetic file catalog.`);
    if (hydrationTestEnabled) {
      await performCloudHydrationTest({
        receiveFrame,
        initiatorKey,
        initiatorIv,
        sendReady: (payload) => sendEncrypted(PKT_WIFI_WAKE, nextSequence++, payload),
      });
    }
    console.log("Encrypted peer traffic sent; check the isolated Windows daemon log for handling.");
  } finally {
    socket.destroy();
  }
}

runSimulation().catch((error) => {
  console.error(`Simulator failed: ${error.message}`);
  process.exitCode = 1;
});

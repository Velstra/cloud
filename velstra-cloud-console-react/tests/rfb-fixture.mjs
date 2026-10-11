// Minimal RFB 3.8 server for exercising the production noVNC client.
// This deliberately uses the real handshake and WebSocket framing.
export function screenFixture(socket, onInput) {
  const send = (payload) => {
    const header = payload.length < 126 ? Buffer.from([0x82, payload.length]) : Buffer.from([0x82, 126, payload.length >> 8, payload.length & 255]);
    socket.write(Buffer.concat([header, payload]));
  };
  let frames = Buffer.alloc(0), input = Buffer.alloc(0), stage = 0;
  send(Buffer.from('RFB 003.008\n'));
  socket.on('data', (chunk) => {
    frames = Buffer.concat([frames, chunk]);
    while (frames.length >= 2) {
      const opcode = frames[0] & 15, masked = !!(frames[1] & 128);
      let length = frames[1] & 127, offset = 2;
      if (length === 126) { if (frames.length < 4) return; length = frames.readUInt16BE(2); offset = 4; }
      if (length === 127) { socket.destroy(); return; }
      if (frames.length < offset + (masked ? 4 : 0) + length) return;
      const mask = masked ? frames.subarray(offset, offset + 4) : null;
      offset += masked ? 4 : 0;
      const payload = Buffer.from(frames.subarray(offset, offset + length));
      frames = frames.subarray(offset + length);
      if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
      if (opcode === 8) { socket.end(Buffer.from([0x88, 0])); return; }
      if (opcode !== 2) continue;
      input = Buffer.concat([input, payload]);
      if (stage === 0 && input.length >= 12) { input = input.subarray(12); stage++; send(Buffer.from([1, 1])); }
      if (stage === 1 && input.length >= 1) { input = input.subarray(1); stage++; send(Buffer.alloc(4)); }
      if (stage === 2 && input.length >= 1) {
        input = input.subarray(1); stage++;
        const name = Buffer.from('Test guest display'), init = Buffer.alloc(24);
        init.writeUInt16BE(320, 0); init.writeUInt16BE(200, 2);
        init[4] = 32; init[5] = 24; init[7] = 1;
        init.writeUInt16BE(255, 8); init.writeUInt16BE(255, 10); init.writeUInt16BE(255, 12);
        init[14] = 16; init[15] = 8; init.writeUInt32BE(name.length, 20);
        send(Buffer.concat([init, name]));
      }
      while (stage === 3 && input.length) {
        const type = input[0];
        let size = ({0: 20, 3: 10, 4: 8, 5: 6})[type];
        if (type === 2) { if (input.length < 4) break; size = 4 + input.readUInt16BE(2) * 4; }
        if (type === 6) { if (input.length < 8) break; size = 8 + input.readUInt32BE(4); }
        if (!size) { socket.destroy(); return; }
        if (input.length < size) break;
        if (type === 4 || type === 5 || type === 6) onInput();
        input = input.subarray(size);
        // A real server waits for damage. Replying to every incremental request
        // synchronously would turn an idle screen into an unbounded busy loop.
        if (type === 3) setTimeout(() => {
          if (!socket.destroyed) send(Buffer.from([0, 0, 0, 0]));
        }, 50);
      }
    }
  });
}

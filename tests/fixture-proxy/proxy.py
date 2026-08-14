import asyncio
from urllib.parse import urlsplit

async def relay(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data); await writer.drain()
    except (ConnectionError, asyncio.CancelledError): pass
    finally:
        writer.close()

async def handle(client_r, client_w):
    try:
        head = await client_r.readuntil(b'\r\n\r\n')
        first = head.split(b'\r\n',1)[0].decode().split()
        if first[0] == 'CONNECT':
            host, port = first[1].rsplit(':',1)
            if host == 'reject.test' or int(port) == 18081:
                client_w.write(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'); await client_w.drain()
                client_w.close(); return
            upstream_r, upstream_w = await asyncio.open_connection(host, int(port))
            client_w.write(b'HTTP/1.1 200 Connection Established\r\n\r\n'); await client_w.drain()
            await asyncio.gather(relay(client_r, upstream_w), relay(upstream_r, client_w))
        else:
            url=urlsplit(first[1])
            if url.hostname == 'error.test' or url.port == 18082:
                client_w.write(b'HTTP/1.1 451 Unavailable For Legal Reasons\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'); await client_w.drain()
                client_w.close(); return
            upstream_r, upstream_w=await asyncio.open_connection(url.hostname,url.port or 80)
            path=(url.path or '/')+(('?'+url.query) if url.query else '')
            upstream_w.write(head.replace(first[1].encode(),path.encode(),1));await upstream_w.drain()
            await asyncio.gather(relay(client_r,upstream_w),relay(upstream_r,client_w))
    except Exception:
        client_w.close()

async def main():
    server=await asyncio.start_server(handle,'0.0.0.0',3128)
    async with server: await server.serve_forever()
asyncio.run(main())

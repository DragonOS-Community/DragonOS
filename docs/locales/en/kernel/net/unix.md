:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/net/unix.md

- Translation time: 2025-09-11 16:37:20

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# UNIX  

## UNIX Socket  

unix - A socket used for inter-process communication  

## Description  

The AF_UNIX socket family is used for communication (IPC) between different processes on the same machine. Currently, UNIX socket addresses support binding to file paths but do not yet support binding to the abstract namespace.  

Currently, valid socket types in the UNIX domain include:  
- **SOCK_STREAM**: Provides a stream-oriented socket that ensures reliable, ordered message transmission.  
- **SOCK_SEQPACKET**: Provides a connection-oriented socket that guarantees message boundaries and delivery in the order sent.  

### UNIX Stream Socket Process Communication Description  

UNIX stream sockets enable stream-based message transmission between processes. Assuming the peer process acts as the server and the local process as the client, the communication process using stream sockets is as follows:  

1. Create a socket in both the peer (server) and local (client) processes. The server must bind an address, while the client is not required to do so.  
2. The communication process is similar to the TCP three-way handshake:  
   - The server calls the `listen` system call to enter a listening state, monitoring the bound address.  
   - The client calls the `connect` system call to connect to the server's address.  
   - The server calls the `accept` system call to accept the client's connection, returning a new socket for the established connection.  
3. After a successful connection, write operations can be performed using `write`, `send`, `sendto`, or `sendmsg`, while read operations can be performed using `read`, `recv`, `recvfrom`, or `recvmsg`.  
4. Non-blocking read/write is not yet supported; the default is blocking read/write.  
5. After reading/writing, the `close` system call is used to terminate the socket connection.  

### UNIX Seqpacket Socket Process Communication Description

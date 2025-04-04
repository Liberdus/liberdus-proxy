# Liberdus proxy server
This is a simple proxy server that forwards requests to the Liberdus consensus node (validators). The service does nothing special except picking the appropriate node to forward the request to. Aims to minimize clients having to track the nodes themselves.

# Have Cargo setup on your system
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
# Clone the repository
```bash
git clone [link]
```
Provide seed archiver in `./src/seed_archiver.json`.

# Standalone network
This is for cases where you have the entire network running on a remote machine that's different than the proxy server, such that archiver will a list of validator with loop back ips since they're on same machine. But the list will break the proxy due to loopback ips. In this case, you can use the standalone network mode.

Configure it in `src/config.json`
```json
{
    "standalone_network": {
        "enabled": true,
        "replacement_ip": "[ip of the machine that house the network you want to connect]"
    }
}
```

Configure the seed archiver in `./src/seed_archiver.json` to have the same ip and credentials.

Note that standalone network may never be used in production.

# Run the server
Make sure you current working directory is inside the root of the repository. And then run the following command.
```bash
cargo run
```

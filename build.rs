use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("proto");

    let proto_files = vec![
        proto_root.join("etcdserverpb/rpc.proto"),
        proto_root.join("etcdserverpb/kv.proto"),
        proto_root.join("etcdserverpb/auth.proto"),
        proto_root.join("raftpb/raft_internal.proto"),
    ];

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(&proto_files, &[proto_root])?;

    Ok(())
}

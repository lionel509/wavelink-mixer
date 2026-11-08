"""One-time export of the UMX-HQ nets for the models-v1 release. Not needed on the PC.
UMX-HQ (MIT, sigsep/open-unmix-pytorch) -> umxhq-<target>.onnx, input "mag" (frames,1,2,2049) magnitude, output "est" same shape.
openunmix 1.x takes (samples, channels, bins, frames); the wrapper permutes to and from that, so the ONNX I/O is the layout src/sep.rs feeds.
Install (main session, Mac): conda create -n umx-export python=3.11 -y && conda run -n umx-export pip install torch --index-url https://download.pytorch.org/whl/cpu openunmix onnx onnxscript"""
import sys, torch, openunmix, onnx, hashlib, pathlib


class MagIO(torch.nn.Module):
    """(frames,1,2,2049) -> openunmix (1,2,2049,frames) -> back to (frames,1,2,2049)."""
    def __init__(self, net):
        super().__init__()
        self.net = net

    def forward(self, mag):
        return self.net(mag.permute(1, 2, 3, 0)).permute(3, 0, 1, 2)


out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "models-v1"); out.mkdir(exist_ok=True)
for t in ["vocals", "drums", "bass", "other"]:
    m = MagIO(openunmix.umxhq(targets=[t], pretrained=True).target_models[t].eval()).eval()
    torch.onnx.export(m, torch.rand(65, 1, 2, 2049), out / f"umxhq-{t}.onnx", opset_version=17,
                      input_names=["mag"], output_names=["est"], dynamic_axes={"mag": {0: "frames"}, "est": {0: "frames"}})
    # The exporter writes the weights to <name>.onnx.data; re-save with them inline so each release asset is one file.
    p = out / f"umxhq-{t}.onnx"
    onnx.save(onnx.load(str(p)), str(p))
    p.with_name(p.name + ".data").unlink(missing_ok=True)
for p in sorted(out.glob("umxhq-*.onnx")): print(p.name, hashlib.sha256(p.read_bytes()).hexdigest(), p.stat().st_size)

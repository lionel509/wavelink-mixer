"""One-time export of the UMX-HQ nets for the models-v1 release. Not needed on the PC.
UMX-HQ (MIT, sigsep/open-unmix-pytorch) -> umxhq-<target>.onnx, input "mag" (frames,1,2,2049) magnitude, output "est" same shape."""
import sys, torch, openunmix, hashlib, pathlib
out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "models-v1"); out.mkdir(exist_ok=True)
for t in ["vocals", "drums", "bass", "other"]:
    m = openunmix.umxhq(targets=[t], pretrained=True).target_models[t].eval()
    torch.onnx.export(m, torch.rand(65, 1, 2, 2049), out / f"umxhq-{t}.onnx", opset_version=17,
                      input_names=["mag"], output_names=["est"], dynamic_axes={"mag": {0: "frames"}, "est": {0: "frames"}})
for p in sorted(out.glob("umxhq-*.onnx")): print(p.name, hashlib.sha256(p.read_bytes()).hexdigest(), p.stat().st_size)

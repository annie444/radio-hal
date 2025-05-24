check:
  #!/usr/bin/env python3
  import subprocess
  import itertools
  esc_start = "\033["
  esc_end = "m"
  esc_join = ";"
  bold = "1"
  bold_reset = "22"
  cyan_fg = "36"
  fg_color_reset = "39"
  tq_out = subprocess.Popen(["tq", ".features", "--file=Cargo.toml", "--output=json"], stdout=subprocess.PIPE)
  jq_out = subprocess.Popen(["jq", "--raw-output", "--monochrome-output", ". | keys | .[]", "-"], stdin=tq_out.stdout, stdout=subprocess.PIPE)
  tq_out.stdout.close()
  features, errors = jq_out.communicate()
  features = list(filter(lambda x : x != '', features.decode().split("\n")))
  print(features)
  for i in range(len(features) + 1):
    for subset in itertools.combinations(features, i):
      feats = ",".join(list(subset))
      print()
      print(esc_start + esc_join.join([bold, cyan_fg]) + esc_end + feats + esc_start + esc_join.join([bold_reset, fg_color_reset]) + esc_end)
      print()
      check = subprocess.Popen(["cargo", "check", "-F", feats], shell=False)
      check.communicate()

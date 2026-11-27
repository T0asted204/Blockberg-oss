#!/bin/bash
# Builds the headless LWJGL stub (lwjgl-agent.jar) that `bblauncher launch --headless` loads.
# It is compiled from the MIT-licensed HeadlessMC project (https://github.com/headlesshq/headlessmc),
# module headlessmc-lwjgl, with ASM relocated to a private package so it cannot clash with Minecraft's own ASM.
#
# Usage: headless/build-headless.sh [output.jar]
#   default output: ~/.local/share/bblauncher/headless/lwjgl-agent.jar  (override the location bblauncher reads with
#   env BBLAUNCHER_LWJGL_AGENT=/path/to/jar)
# Needs: git, curl, a JDK (javac/jar) 17+.
set -euo pipefail
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
out="${1:-${XDG_DATA_HOME:-$HOME/.local/share}/bblauncher/headless/lwjgl-agent.jar}"
HMC_COMMIT="${HMC_COMMIT:-023738fb565d5f5b499707738a5cd491df334985}"   # audited revision (3.0.0-RC8)
ASM=9.11.1; LOMBOK=1.18.34; JSPECIFY=1.0.0
M=https://repo1.maven.org/maven2
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
cd "$work"

echo "fetching HeadlessMC $HMC_COMMIT"
git init -q hmc && git -C hmc remote add origin https://github.com/headlesshq/headlessmc.git
git -C hmc fetch -q --depth 1 origin "$HMC_COMMIT" && git -C hmc checkout -q FETCH_HEAD

mkdir lib
for j in "org/ow2/asm/asm/$ASM/asm-$ASM.jar" "org/ow2/asm/asm-tree/$ASM/asm-tree-$ASM.jar" \
         "org/ow2/asm/asm-commons/$ASM/asm-commons-$ASM.jar" "org/projectlombok/lombok/$LOMBOK/lombok-$LOMBOK.jar" \
         "org/jspecify/jspecify/$JSPECIFY/jspecify-$JSPECIFY.jar"; do
  curl -fsSL -o "lib/$(basename "$j")" "$M/$j"
done
CP="lib/asm-$ASM.jar:lib/asm-tree-$ASM.jar:lib/asm-commons-$ASM.jar:lib/jspecify-$JSPECIFY.jar"

echo "compiling"
# the launchwrapper/ classes only serve old Forge (LaunchWrapper) and need its jar; Fabric does not use them
find hmc/headlessmc-lwjgl/src/main/java -name '*.java' ! -path '*/launchwrapper/*' > sources.txt
javac --release 8 -nowarn -encoding UTF-8 -proc:full -cp "$CP:lib/lombok-$LOMBOK.jar" -processorpath "lib/lombok-$LOMBOK.jar" \
  -d classes @sources.txt || { echo "compile failed"; exit 1; }
[ -d hmc/headlessmc-lwjgl/src/main/resources ] && cp -r hmc/headlessmc-lwjgl/src/main/resources/. classes/

echo "relocating ASM"
mkdir asm && (cd asm && for j in asm asm-tree; do jar xf "../lib/$j-$ASM.jar"; done; rm -rf META-INF module-info.class)
cp -r asm/. classes/
javac -nowarn -cp "$CP" -d reloc "$here/Relocate.java"
java -cp "reloc:$CP" Relocate classes relocated
# keep non-class resources
(cd classes && find . -type f ! -name '*.class' -exec cp --parents {} ../relocated/ \;)

cat > manifest.txt <<'EOF'
Premain-Class: io.github.headlesshq.headlessmc.lwjgl.agent.LwjglAgent
Agent-Class: io.github.headlesshq.headlessmc.lwjgl.agent.LwjglAgent
Can-Redefine-Classes: true
Can-Retransform-Classes: true
EOF
mkdir -p relocated/META-INF && cp hmc/LICENSE relocated/META-INF/LICENSE-headlessmc.txt
mkdir -p "$(dirname "$out")"
jar cfm "$out" manifest.txt -C relocated .
echo "built $out"

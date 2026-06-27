set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
if [ ! -f "/app/crossfiles/cross.txt" ]; then
tee /app/crossfiles/cross.txt << EOS
[binaries]
c = '${CC}'
cpp = '${CXX}'
ar = '${AR}'

[host_machine]
system = 'linux'
cpu_family = '${CPU_FAMILY}'
cpu = '${CPU}'
endian = '${ENDIAN}'
EOS
fi

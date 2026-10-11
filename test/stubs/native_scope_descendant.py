"""All descendants belong to a temporary owned provider Job; no model or user Agent."""
import os
from pathlib import Path
import subprocess
import sys
import time

directory = Path(sys.argv[1])
kind = sys.argv[2]
(directory / (kind + '.pid')).write_text(str(os.getpid()))
if kind == 'child':
    subprocess.Popen([sys.executable, '-I', __file__, str(directory), 'grandchild'],
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
while not (directory / 'release-fault').exists(): time.sleep(.025)

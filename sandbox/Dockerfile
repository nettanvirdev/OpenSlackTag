# The teammate's own computer: what `run_shell` commands run in.
#
#   docker build -t openslacktag-sandbox sandbox/
#
# Then set SANDBOX_IMAGE=openslacktag-sandbox. It has what everyday office and
# data work needs: Python with data, chart and document libraries (Excel,
# Word, PowerPoint, PDF), plus common command-line tools.
FROM python:3.12-slim-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        curl jq git unzip zip poppler-utils sqlite3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN pip install --no-cache-dir \
        pandas numpy matplotlib openpyxl xlsxwriter python-docx python-pptx \
        pypdf reportlab requests beautifulsoup4 tabulate pyyaml

# Commands run as the host user (or root on Docker Desktop) in /work, the
# conversation's workspace.
ENV HOME=/work MPLCONFIGDIR=/tmp/matplotlib
WORKDIR /work

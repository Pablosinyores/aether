FROM python:3.12-alpine

WORKDIR /app
COPY scripts/canary.py /app/canary.py

USER nobody
CMD ["python", "/app/canary.py"]
